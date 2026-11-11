//! 업체 호출 기록 (docs/05-observability-and-analytics.md §6)
//!
//! ```text
//! 시도 완료 ──try_send──▶ [mpsc 채널] ──▶ 기록 태스크 (booking-server가 배치로 provider_calls에 INSERT)
//! 채널이 가득 차면: 기다리지 않고 버리고 dropped를 1 올린다
//! ```
//! 이 크레이트는 채널의 보내는 쪽만 가진다. DB에 쓰는 받는 쪽은 booking-server에 있다.
//! (providers는 db 크레이트에 의존하지 않는다: docs/01-architecture.md §6)
//!
//! `mpsc`는 "여러 생산자, 하나의 소비자" 채널이다 (≈ C#의 `Channel.CreateBounded<T>`).
//! 생산자(`Sender`)는 복제해서 여러 곳에서 쓰고, 소비자(`Receiver`)는 하나만 있다.
//! 모든 `Sender`가 사라지면 소비자의 `recv()`가 `None`을 돌려줘서 끝났다는 것을 안다.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use domain::{BookingId, ProviderId};
use tokio::sync::mpsc::{self, error::TrySendError};

/// 시도 한 번의 기록 (`provider_calls` 한 행)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    pub provider_id: ProviderId,
    /// `create_booking`, `find_booking` …
    pub operation: &'static str,
    pub booking_id: Option<BookingId>,
    pub job_id: Option<i64>,
    /// 호출 내 재시도 번호 (1부터)
    pub attempt: u32,
    pub started_at: DateTime<Utc>,
    pub duration: Duration,
    /// `success` 또는 `ProviderError::outcome()`
    pub outcome: &'static str,
    pub http_status: Option<u16>,
    /// 업체 응답 헤더(`X-RateLimit-Remaining`)에서 읽은 값
    pub rate_limit_remaining: Option<u32>,
    /// 업체가 `Retry-After`로 알려 준 대기 시간
    pub retry_after: Option<Duration>,
    pub error_detail: Option<String>,
    /// 이 시도가 속한 trace. 관리자 화면의 호출 기록에서 Grafana의 trace로 넘어갈 때 쓴다.
    pub trace_id: Option<String>,
}

/// 호출 기록을 보내는 쪽. 복제해서 업체마다 하나씩 나눠 준다.
///
/// `#[derive(Clone)]`으로 복제해도 `Sender`는 같은 채널을, `Arc<AtomicU64>`는 같은 카운터를 가리킨다.
#[derive(Debug, Clone)]
pub struct CallRecorder {
    sender: mpsc::Sender<CallRecord>,
    /// 채널이 가득 차서 버린 건수. `AtomicU64`는 잠금 없이 여러 스레드가 함께 올릴 수 있는 정수다 (≈ `Interlocked.Increment`)
    dropped: Arc<AtomicU64>,
}

/// 최대 `capacity`건을 쌓아 둘 수 있는 기록 채널을 만든다
pub fn call_recorder(capacity: usize) -> (CallRecorder, mpsc::Receiver<CallRecord>) {
    let (sender, receiver) = mpsc::channel(capacity.max(1));
    crate::metrics::register_recorder();
    let recorder = CallRecorder {
        sender,
        dropped: Arc::new(AtomicU64::new(0)),
    };
    (recorder, receiver)
}

impl CallRecorder {
    /// 기록을 버리는 recorder (기록이 필요 없는 테스트용). 받는 쪽이 없으므로 보낸 기록은 바로 버려진다.
    pub fn discard() -> Self {
        call_recorder(1).0
    }

    /// 기록을 보낸다. **기다리지 않는다**: 채널이 가득 차면 버린다.
    /// 관측 때문에 업체 호출이 느려지면 안 되기 때문이다 (CLAUDE.md 불변 조건).
    pub fn record(&self, record: CallRecord) {
        match self.sender.try_send(record) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                let before = self.dropped.fetch_add(1, Ordering::Relaxed);
                crate::metrics::record_dropped();
                // 버릴 때마다 로그를 남기면 그것도 부하가 되므로 처음과 1000건마다만 남긴다
                if before.is_multiple_of(1000) {
                    tracing::warn!(
                        dropped = before + 1,
                        "호출 기록 채널이 가득 차 기록을 버렸다"
                    );
                }
            }
            // 받는 쪽이 이미 끝났다 (종료 중이거나 discard). 조용히 버린다.
            Err(TrySendError::Closed(_)) => {}
        }
    }

    /// 지금까지 버린 건수
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(attempt: u32) -> CallRecord {
        CallRecord {
            provider_id: ProviderId::parse("alpha").unwrap(),
            operation: "create_booking",
            booking_id: None,
            job_id: None,
            attempt,
            started_at: Utc::now(),
            duration: Duration::from_millis(10),
            outcome: "success",
            http_status: Some(201),
            rate_limit_remaining: Some(9),
            retry_after: None,
            error_detail: None,
            trace_id: None,
        }
    }

    #[test]
    fn 채널이_가득_차면_기다리지_않고_버린다() {
        let (recorder, mut receiver) = call_recorder(2);
        for attempt in 1..=5 {
            recorder.record(record(attempt));
        }
        assert_eq!(recorder.dropped(), 3);
        assert_eq!(receiver.try_recv().unwrap().attempt, 1);
        assert_eq!(receiver.try_recv().unwrap().attempt, 2);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn 받는_쪽이_없으면_조용히_버린다() {
        let recorder = CallRecorder::discard();
        recorder.record(record(1));
        recorder.record(record(2));
        assert_eq!(recorder.dropped(), 0, "가득 차서 버린 것이 아니다");
    }

    #[test]
    fn 복제한_recorder는_같은_채널로_보낸다() {
        let (recorder, mut receiver) = call_recorder(10);
        let clone = recorder.clone();
        recorder.record(record(1));
        clone.record(record(2));
        drop((recorder, clone));

        let mut attempts = Vec::new();
        while let Some(record) = receiver.blocking_recv() {
            attempts.push(record.attempt);
        }
        assert_eq!(
            attempts,
            vec![1, 2],
            "보낸 쪽이 모두 사라지면 recv가 끝난다"
        );
    }
}
