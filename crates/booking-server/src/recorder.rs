//! 업체 호출 기록 저장 태스크 (docs/05-observability-and-analytics.md §6)
//!
//! ```text
//! ResilientProvider ──try_send──▶ [mpsc 채널, 용량 10,000] ──▶ run() ──▶ provider_calls
//!                                                          (1초마다 또는 500건마다 배치 INSERT)
//! ```
//! 업체 호출 경로는 채널에 넣기만 하고 기다리지 않는다. DB가 느리거나 실패해도 예약 처리는 영향을 받지 않는다.
//! 대신 저장에 실패한 기록은 버린다 (기록은 분석용이고, 예약 상태의 원본은 bookings · booking_events다).

use std::time::Duration;

use db::{PgPool, provider_calls::NewProviderCall};
use providers::CallRecord;
use tokio::sync::mpsc;

/// 채널에 쌓아 둘 수 있는 최대 건수. 넘치면 보내는 쪽이 버린다.
pub const CHANNEL_CAPACITY: usize = 10_000;
/// 한 번에 넣는 최대 건수
const BATCH_SIZE: usize = 500;
/// 첫 기록을 받은 뒤 이만큼 더 모았다가 넣는다
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// 채널이 닫힐 때까지(보내는 쪽이 모두 사라질 때까지) 기록을 모아 저장한다.
///
/// 종료할 때는 워커가 먼저 멈추고 업체 파이프라인(보내는 쪽)이 사라지면, 남은 기록을 마저 저장하고 끝난다.
pub async fn run(pool: PgPool, mut receiver: mpsc::Receiver<CallRecord>) {
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    // 첫 기록이 올 때까지 기다린다. `None`이면 채널이 닫혔다.
    while let Some(first) = receiver.recv().await {
        batch.push(first);
        // 1초가 지나거나 500건이 찰 때까지 더 모은다
        let flush_at = tokio::time::Instant::now() + FLUSH_INTERVAL;
        while batch.len() < BATCH_SIZE {
            match tokio::time::timeout_at(flush_at, receiver.recv()).await {
                Ok(Some(record)) => batch.push(record),
                // Ok(None): 채널이 닫힘, Err(_): 시간이 다 됨
                Ok(None) | Err(_) => break,
            }
        }
        flush(&pool, &mut batch).await;
    }
    tracing::debug!("call recorder stopped");
}

/// 모은 기록을 저장하고 비운다. 실패하면 로그만 남기고 버린다.
async fn flush(pool: &PgPool, batch: &mut Vec<CallRecord>) {
    // `drain(..)`: 벡터의 항목을 꺼내면서 비운다. 벡터가 잡아 둔 메모리는 다음 배치에 다시 쓴다.
    let calls: Vec<NewProviderCall> = batch.drain(..).map(to_row).collect();
    if let Err(error) = db::provider_calls::insert_batch(pool, &calls).await {
        tracing::error!(%error, count = calls.len(), "호출 기록을 저장하지 못해 버린다");
    }
}

fn to_row(record: CallRecord) -> NewProviderCall {
    NewProviderCall {
        provider_id: record.provider_id.to_string(),
        operation: record.operation.to_owned(),
        booking_id: record.booking_id,
        job_id: record.job_id,
        attempt: saturating_i32(u128::from(record.attempt)),
        started_at: db::normalize_timestamp(record.started_at),
        duration_ms: saturating_i32(record.duration.as_millis()),
        outcome: record.outcome.to_owned(),
        http_status: record.http_status.map(i32::from),
        rate_limit_remaining: record
            .rate_limit_remaining
            .map(|n| saturating_i32(u128::from(n))),
        retry_after_ms: record
            .retry_after
            .map(|wait| saturating_i32(wait.as_millis())),
        error_detail: record.error_detail,
    }
}

/// DB 컬럼(`integer`)에 들어가도록 너무 큰 값은 최댓값으로 자른다
fn saturating_i32(value: u128) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use domain::ProviderId;

    use super::*;

    #[test]
    fn 기록을_db_행으로_바꾼다() {
        let record = CallRecord {
            provider_id: ProviderId::parse("alpha").unwrap(),
            operation: "create_booking",
            booking_id: None,
            job_id: Some(3),
            attempt: 2,
            started_at: Utc::now(),
            duration: Duration::from_micros(1_500),
            outcome: "rate_limited",
            http_status: Some(429),
            rate_limit_remaining: Some(u32::MAX),
            retry_after: Some(Duration::from_secs(1)),
            error_detail: Some("호출 한도 초과".into()),
        };
        let row = to_row(record);
        assert_eq!(row.provider_id, "alpha");
        assert_eq!(row.attempt, 2);
        assert_eq!(row.duration_ms, 1, "밀리초 미만은 버린다");
        assert_eq!(row.http_status, Some(429));
        assert_eq!(
            row.rate_limit_remaining,
            Some(i32::MAX),
            "너무 큰 값은 자른다"
        );
        assert_eq!(row.retry_after_ms, Some(1_000));
    }
}
