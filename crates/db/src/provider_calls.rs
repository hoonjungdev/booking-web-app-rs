//! 외부 업체 호출 기록 (`provider_calls`)
//!
//! 쓰기는 booking-server의 기록 태스크가 모아서 [`insert_batch`]로 한 번에 넣는다.

use chrono::{DateTime, Utc};
use domain::BookingId;

use crate::{DbError, PgPool};

/// 저장할 호출 기록 한 건
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProviderCall {
    pub provider_id: String,
    pub operation: String,
    pub booking_id: Option<BookingId>,
    pub job_id: Option<i64>,
    pub attempt: i32,
    pub started_at: DateTime<Utc>,
    pub duration_ms: i32,
    pub outcome: String,
    pub http_status: Option<i32>,
    pub rate_limit_remaining: Option<i32>,
    pub retry_after_ms: Option<i32>,
    pub error_detail: Option<String>,
    pub trace_id: Option<String>,
}

/// 저장된 호출 기록
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCallRecord {
    pub id: i64,
    pub provider_id: String,
    pub operation: String,
    pub booking_id: Option<BookingId>,
    pub job_id: Option<i64>,
    pub attempt: i32,
    pub started_at: DateTime<Utc>,
    pub duration_ms: i32,
    pub outcome: String,
    pub http_status: Option<i32>,
    pub rate_limit_remaining: Option<i32>,
    pub retry_after_ms: Option<i32>,
    pub error_detail: Option<String>,
    /// 이 호출이 속한 trace. Grafana에서 그 호출의 앞뒤(재시도, 업체 내부 처리)를 볼 수 있다.
    pub trace_id: Option<String>,
}

/// 여러 건을 INSERT 한 번으로 넣는다. 넣은 행 수를 돌려준다.
///
/// 열마다 값을 배열 하나로 모아 보내고, PostgreSQL의 `UNNEST`가 배열들을 다시 행으로 펼친다.
/// 그래서 건수와 상관없이 SQL 문장이 하나라서 `query!`로 컴파일 시점에 검사할 수 있다.
///
/// 값이 없는 칸은 배열 안에 `NULL`로 넣는다(`Vec<Option<T>>`). `query!`는 배열 매개변수를 `&[T]`로 기대해서
/// `Option` 원소를 거절하므로, 그런 열은 `값 as &[Option<T>]`로 타입을 직접 알려 준다 (sqlx의 매개변수 타입 지정 문법).
pub async fn insert_batch(pool: &PgPool, calls: &[NewProviderCall]) -> Result<u64, DbError> {
    if calls.is_empty() {
        return Ok(0);
    }
    // 반복자에서 열 하나씩 뽑아 Vec으로 모은다 (≈ LINQ의 `calls.Select(c => c.ProviderId).ToList()`)
    let provider_ids: Vec<String> = calls.iter().map(|c| c.provider_id.clone()).collect();
    let operations: Vec<String> = calls.iter().map(|c| c.operation.clone()).collect();
    let booking_ids: Vec<Option<uuid::Uuid>> = calls
        .iter()
        .map(|c| c.booking_id.map(|id| id.as_uuid()))
        .collect();
    let job_ids: Vec<Option<i64>> = calls.iter().map(|c| c.job_id).collect();
    let attempts: Vec<i32> = calls.iter().map(|c| c.attempt).collect();
    let started_ats: Vec<DateTime<Utc>> = calls.iter().map(|c| c.started_at).collect();
    let durations: Vec<i32> = calls.iter().map(|c| c.duration_ms).collect();
    let outcomes: Vec<String> = calls.iter().map(|c| c.outcome.clone()).collect();
    let http_statuses: Vec<Option<i32>> = calls.iter().map(|c| c.http_status).collect();
    let remainings: Vec<Option<i32>> = calls.iter().map(|c| c.rate_limit_remaining).collect();
    let retry_afters: Vec<Option<i32>> = calls.iter().map(|c| c.retry_after_ms).collect();
    let details: Vec<Option<String>> = calls.iter().map(|c| c.error_detail.clone()).collect();
    let trace_ids: Vec<Option<String>> = calls.iter().map(|c| c.trace_id.clone()).collect();

    let result = sqlx::query!(
        r#"
        INSERT INTO provider_calls (provider_id, operation, booking_id, job_id, attempt, started_at,
                                    duration_ms, outcome, http_status, rate_limit_remaining,
                                    retry_after_ms, error_detail, trace_id)
        SELECT * FROM UNNEST($1::text[], $2::text[], $3::uuid[], $4::int8[], $5::int4[],
                             $6::timestamptz[], $7::int4[], $8::text[], $9::int4[], $10::int4[],
                             $11::int4[], $12::text[], $13::text[])
        "#,
        &provider_ids,
        &operations,
        &booking_ids as &[Option<uuid::Uuid>],
        &job_ids as &[Option<i64>],
        &attempts,
        &started_ats,
        &durations,
        &outcomes,
        &http_statuses as &[Option<i32>],
        &remainings as &[Option<i32>],
        &retry_afters as &[Option<i32>],
        &details as &[Option<String>],
        &trace_ids as &[Option<String>],
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// 예약 하나에 대한 호출 기록 (오래된 것부터). 관리자 화면의 예약 상세 · 테스트용.
pub async fn for_booking(
    pool: &PgPool,
    booking_id: BookingId,
) -> Result<Vec<ProviderCallRecord>, DbError> {
    let rows = sqlx::query!(
        "SELECT id, provider_id, operation, booking_id, job_id, attempt, started_at, duration_ms,
                outcome, http_status, rate_limit_remaining, retry_after_ms, error_detail, trace_id
         FROM provider_calls WHERE booking_id = $1 ORDER BY id",
        booking_id.as_uuid(),
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| ProviderCallRecord {
            id: r.id,
            provider_id: r.provider_id,
            operation: r.operation,
            booking_id: r.booking_id.map(BookingId::from_uuid),
            job_id: r.job_id,
            attempt: r.attempt,
            started_at: r.started_at,
            duration_ms: r.duration_ms,
            outcome: r.outcome,
            http_status: r.http_status,
            rate_limit_remaining: r.rate_limit_remaining,
            retry_after_ms: r.retry_after_ms,
            error_detail: r.error_detail,
            trace_id: r.trace_id,
        })
        .collect())
}
