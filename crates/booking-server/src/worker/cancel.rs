//! `cancel_booking` 작업: 취소 요청을 받은(`cancel_requested`) 예약을 업체에서 취소한다 (Phase 6)
//!
//! ```text
//! 업체 취소 ─┬─ 성공 (이미 취소된 예약 포함) ──▶ cancelled
//!           ├─ 업무적 거절 (업체에 없는 예약 등) ─▶ cancel_failed (관리자 확인)
//!           ├─ 우리 쪽에서 막힘 (서킷 · pause) ────▶ 시도 횟수를 쓰지 않고 미룸
//!           └─ 그 밖의 실패 ──────────────────────▶ 작업 재예약, 횟수 소진 시 cancel_failed
//! ```
//!
//! 취소는 생성과 달리 결과 불명 상태가 없다. 업체가 처리했는지 모르는 실패(타임아웃 · 5xx)여도 다시 보낸다:
//! 이미 취소된 예약을 다시 취소해도 결과가 같다 (어댑터가 "이미 취소됨"을 성공으로 분류. docs/decisions.md D-32).

use std::time::Instant;

use db::jobs::Job;
use domain::{BookingEvent, BookingStatus};
use providers::{CallContext, ProviderError};

use super::{
    Decision, WorkerContext, WorkerError, apply, complete_only, defer_if_blocked, give_up,
};

/// 업체 취소 결과 → 다음 행동 (순수 함수)
pub fn decide_cancel(result: Result<(), ProviderError>, now: Instant) -> Decision {
    let error = match result {
        Ok(()) => return Decision::Transition(BookingEvent::CancelConfirmed),
        Err(error) => error,
    };
    if let Some(defer) = defer_if_blocked(&error, now) {
        return defer;
    }
    match error {
        // 업체가 취소를 받지 않았다 (업체에 그 예약이 없는 등). 다시 보내도 같으므로 관리자가 보게 한다.
        ProviderError::Rejected { reason } => Decision::GiveUp(reason.code()),
        // 예상하지 못한 4xx: 대개 우리 쪽 버그
        error @ ProviderError::Client { .. } => Decision::GiveUp(error.to_string()),
        // 취소는 다시 보내도 결과가 같으므로 처리 여부를 모르는 실패도 다시 시도한다
        error => Decision::Retry(error.to_string()),
    }
}

pub(super) async fn handle(ctx: &WorkerContext, job: &Job) -> Result<(), WorkerError> {
    let lease = job.lease();
    let Some(booking) = super::load_booking(ctx, job).await? else {
        return Ok(());
    };
    if booking.status != BookingStatus::CancelRequested {
        tracing::info!(status = %booking.status, "취소 중인 예약이 아니라 작업만 끝낸다");
        return complete_only(ctx, lease).await;
    }
    let Some(provider) = ctx.providers.get(&booking.request.provider) else {
        let reason = format!("provider_not_configured: {}", booking.request.provider);
        return give_up(ctx, lease, &booking, reason).await;
    };
    let Some(provider_booking_id) = booking.provider_booking_id.as_deref() else {
        // 확정된 예약에는 업체 예약 ID가 있어야 한다. 없으면 무엇을 취소할지 알 수 없어 관리자에게 넘긴다.
        let reason = "missing_provider_booking_id: 업체 예약 ID가 없어 취소할 수 없다".to_owned();
        return give_up(ctx, lease, &booking, reason).await;
    };

    let call = CallContext {
        booking_id: Some(booking.id),
        job_id: Some(job.id),
    };
    let result = provider.cancel_booking(provider_booking_id, call).await;
    match &result {
        Ok(()) => tracing::info!("업체에서 예약을 취소했다"),
        Err(error) if error.blocked_until().is_some() => {}
        Err(error) => {
            tracing::warn!(outcome = error.outcome(), %error, "업체에서 예약을 취소하지 못했다")
        }
    }
    let decision = decide_cancel(result, Instant::now());
    apply(ctx, job, &booking, decision).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use domain::RejectReason;

    use super::*;

    #[test]
    fn 성공하면_취소_완료() {
        assert_eq!(
            decide_cancel(Ok(()), Instant::now()),
            Decision::Transition(BookingEvent::CancelConfirmed)
        );
    }

    #[test]
    fn 처리_여부를_모르는_실패도_다시_보낸다() {
        for error in [
            ProviderError::Timeout,
            ProviderError::Server { status: 503 },
            ProviderError::Transport {
                detail: "reset".into(),
            },
            ProviderError::Connect {
                detail: "refused".into(),
            },
        ] {
            assert!(
                matches!(
                    decide_cancel(Err(error.clone()), Instant::now()),
                    Decision::Retry(_)
                ),
                "{error:?}"
            );
        }
    }

    #[test]
    fn 업체가_취소를_받지_않으면_관리자에게_넘긴다() {
        let missing = ProviderError::Rejected {
            reason: RejectReason::BookingNotFound,
        };
        assert_eq!(
            decide_cancel(Err(missing), Instant::now()),
            Decision::GiveUp("booking_not_found".into())
        );
        let bug = ProviderError::Client {
            status: 400,
            body: String::new(),
        };
        assert!(matches!(
            decide_cancel(Err(bug), Instant::now()),
            Decision::GiveUp(_)
        ));
    }

    #[test]
    fn 우리_쪽에서_막히면_미룬다() {
        let now = Instant::now();
        let blocked = ProviderError::CircuitOpen {
            retry_at: now + Duration::from_secs(3),
        };
        assert!(matches!(
            decide_cancel(Err(blocked), now),
            Decision::Defer { wait, .. } if wait == Duration::from_secs(3)
        ));
    }
}
