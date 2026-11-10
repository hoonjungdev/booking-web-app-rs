//! `submit_booking` 작업: 접수된 예약을 업체에 제출한다

use std::time::Instant;

use chrono::Utc;
use db::jobs::Job;
use domain::{BookingEvent, BookingStatus};
use providers::{CallContext, Capabilities, CreateBooking, ProviderBooking, ProviderError};

use super::{
    Decision, WorkerContext, WorkerError, apply, complete_only, defer_if_blocked, give_up,
};

/// 업체 호출 결과 → 다음 행동. DB도 네트워크도 쓰지 않는 순수 함수라서 단위 테스트로 모든 경우를 확인한다.
///
/// 결과는 `ResilientProvider`가 호출 내 재시도까지 마친 최종 결과다.
///
/// 이중 예약을 막는 핵심 규칙 (docs/03-resilience.md §5):
/// 업체가 처리했는지 알 수 없는 실패(타임아웃 · 5xx · 전송 오류 · 응답 해석 실패)는
/// **멱등키를 지원하는 업체에만** 다시 보낸다. 지원하지 않으면 다시 보내지 않고 결과 불명(`unknown`)으로 둔다.
pub fn decide(
    result: Result<ProviderBooking, ProviderError>,
    capabilities: Capabilities,
    now: Instant,
) -> Decision {
    let error = match result {
        Ok(booking) => {
            return Decision::Transition(BookingEvent::ProviderConfirmed {
                provider_booking_id: booking.provider_booking_id,
            });
        }
        Err(error) => error,
    };
    // 서킷 열림 · 대기 한도 초과 · 업체 pause: 업체를 부르지 않았으므로 시도 횟수를 쓰지 않고 미룬다
    if let Some(defer) = defer_if_blocked(&error, now) {
        return defer;
    }
    match error {
        ProviderError::Rejected { reason } => {
            Decision::Transition(BookingEvent::ProviderRejected { reason })
        }
        // 예상하지 못한 4xx는 다시 보내도 같은 결과라서(대개 우리 버그) 포기하고 관리자가 보게 한다
        error @ ProviderError::Client { .. } => Decision::GiveUp(error.to_string()),
        error if error.definitely_not_processed() || capabilities.idempotency_key => {
            Decision::Retry(error.to_string())
        }
        error => Decision::Transition(BookingEvent::OutcomeUnknown {
            detail: error.to_string(),
        }),
    }
}

pub(super) async fn handle(ctx: &WorkerContext, job: &Job) -> Result<(), WorkerError> {
    let lease = job.lease();
    let Some(booking) = super::load_booking(ctx, job).await? else {
        return Ok(());
    };
    if booking.status != BookingStatus::Pending {
        // 다른 경로로 이미 처리됨 (lease 만료로 다른 워커가 먼저 끝낸 경우 등). 작업만 끝낸다.
        tracing::info!(status = %booking.status, "이미 처리된 예약이라 작업만 끝낸다");
        return complete_only(ctx, lease).await;
    }
    if Utc::now() >= booking.deadline_at {
        return give_up(ctx, lease, &booking, "deadline_exceeded".to_owned()).await;
    }
    let Some(provider) = ctx.providers.get(&booking.request.provider) else {
        let reason = format!("provider_not_configured: {}", booking.request.provider);
        return give_up(ctx, lease, &booking, reason).await;
    };

    let call = CallContext {
        booking_id: Some(booking.id),
        job_id: Some(job.id),
    };
    let result = provider
        .create_booking(&CreateBooking::from_booking(&booking), call)
        .await;
    // 우리 쪽에서 막혀 부르지 않은 경우(서킷 · pause)는 apply가 "미룬다"로 남기므로, 실제로 부른 실패만 경고한다
    if let Err(error) = &result
        && error.blocked_until().is_none()
    {
        tracing::warn!(outcome = error.outcome(), %error, "업체에 예약을 만들지 못했다");
    }

    let decision = decide(result, provider.capabilities(), Instant::now());
    apply(ctx, job, &booking, decision).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use domain::RejectReason;

    use super::*;

    const IDEMPOTENT: Capabilities = Capabilities {
        idempotency_key: true,
        lookup_by_reference: true,
        list_bookings: true,
    };
    const NOT_IDEMPOTENT: Capabilities = Capabilities {
        idempotency_key: false,
        lookup_by_reference: true,
        list_bookings: false,
    };

    fn decide_now(
        result: Result<ProviderBooking, ProviderError>,
        capabilities: Capabilities,
    ) -> Decision {
        decide(result, capabilities, Instant::now())
    }

    /// 처리 여부를 알 수 없는 실패들
    fn ambiguous_failures() -> Vec<ProviderError> {
        vec![
            ProviderError::Timeout,
            ProviderError::Transport {
                detail: "reset".into(),
            },
            ProviderError::Server { status: 502 },
            ProviderError::Decode {
                detail: "bad json".into(),
            },
        ]
    }

    #[test]
    fn 확정과_거절은_상태를_바꾼다() {
        let ok = decide_now(
            Ok(ProviderBooking {
                provider_booking_id: "rsv_1".into(),
            }),
            IDEMPOTENT,
        );
        assert_eq!(
            ok,
            Decision::Transition(BookingEvent::ProviderConfirmed {
                provider_booking_id: "rsv_1".into()
            })
        );

        let rejected = decide_now(
            Err(ProviderError::Rejected {
                reason: RejectReason::SlotUnavailable,
            }),
            NOT_IDEMPOTENT,
        );
        assert_eq!(
            rejected,
            Decision::Transition(BookingEvent::ProviderRejected {
                reason: RejectReason::SlotUnavailable
            })
        );
    }

    #[test]
    fn 처리_여부를_모르는_실패는_멱등키가_있을_때만_재시도한다() {
        for error in ambiguous_failures() {
            assert!(
                matches!(
                    decide_now(Err(error.clone()), IDEMPOTENT),
                    Decision::Retry(_)
                ),
                "{error:?} + 멱등키 지원 → 재시도"
            );
            assert!(
                matches!(
                    decide_now(Err(error.clone()), NOT_IDEMPOTENT),
                    Decision::Transition(BookingEvent::OutcomeUnknown { .. })
                ),
                "{error:?} + 멱등키 미지원 → 결과 불명 (다시 보내면 이중 예약 위험)"
            );
        }
    }

    #[test]
    fn 요청이_가지_않은_실패는_멱등키가_없어도_재시도한다() {
        let not_sent = [
            ProviderError::Connect {
                detail: "refused".into(),
            },
            ProviderError::RateLimited { retry_after: None },
        ];
        for error in not_sent {
            assert!(matches!(
                decide_now(Err(error), NOT_IDEMPOTENT),
                Decision::Retry(_)
            ));
        }
    }

    #[test]
    fn 우리_쪽에서_막힌_호출은_시도_횟수를_쓰지_않고_미룬다() {
        let now = Instant::now();
        let later = now + Duration::from_secs(30);
        for error in [
            ProviderError::CircuitOpen { retry_at: later },
            ProviderError::Throttled { retry_at: later },
        ] {
            assert_eq!(
                decide(Err(error.clone()), NOT_IDEMPOTENT, now),
                Decision::Defer {
                    wait: Duration::from_secs(30),
                    reason: error.to_string()
                }
            );
        }
        // 이미 지난 시각이면 바로 다시 해 볼 수 있다
        let past = decide(
            Err(ProviderError::Throttled { retry_at: now }),
            IDEMPOTENT,
            later,
        );
        assert!(matches!(past, Decision::Defer { wait, .. } if wait.is_zero()));
    }

    #[test]
    fn 예상하지_못한_4xx는_포기한다() {
        let error = ProviderError::Client {
            status: 422,
            body: "{}".into(),
        };
        assert!(matches!(
            decide_now(Err(error), IDEMPOTENT),
            Decision::GiveUp(_)
        ));
    }
}
