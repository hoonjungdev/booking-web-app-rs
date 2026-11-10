//! `resolve_unknown` 작업: 결과 불명(`unknown`)인 예약을 업체에서 조회해 확인한다 (docs/03-resilience.md §5)
//!
//! ```text
//! external_ref로 조회 ─┬─ 있음 ─────────────────────────▶ confirmed
//!                     ├─ 없음 (간격을 두고 2번 확인) ──▶ pending으로 되돌리고 다시 제출 (submit_booking 등록)
//!                     └─ 조회 실패 ───────────────────▶ 작업 재예약, 횟수 소진 시 failed
//! ```
//! "없음"을 한 번만 보고 다시 제출하지 않는 이유: 업체 내부 처리가 늦어 조회에 아직 안 보일 수 있다.
//! 그때 다시 제출하면 이중 예약이 된다. 확인 횟수는 작업의 `payload`에 남긴다.

use std::time::Instant;

use db::jobs::Job;
use domain::{BookingEvent, BookingStatus};
use providers::{CallContext, ProviderBooking, ProviderError};
use serde::Deserialize;

use super::{
    Decision, WorkerContext, WorkerError, apply, complete_only, defer_if_blocked, give_up,
};

/// 업체에 없다는 것을 이만큼 확인해야 다시 제출한다
pub const REQUIRED_NOT_FOUND_CHECKS: u32 = 2;

/// 작업 `payload`에 남기는 진행 상황
#[derive(Debug, Default, Deserialize)]
struct Progress {
    #[serde(default)]
    not_found_checks: u32,
}

/// 조회 결과 → 다음 행동 (순수 함수). `not_found_checks`는 이전 실행에서 "없음"을 확인한 횟수.
pub fn decide_resolution(
    result: Result<Option<ProviderBooking>, ProviderError>,
    not_found_checks: u32,
    now: Instant,
) -> Decision {
    let error = match result {
        Ok(Some(found)) => {
            return Decision::Transition(BookingEvent::FoundAtProvider {
                provider_booking_id: found.provider_booking_id,
            });
        }
        Ok(None) => {
            let checks = not_found_checks + 1;
            return if checks >= REQUIRED_NOT_FOUND_CHECKS {
                Decision::Transition(BookingEvent::NotFoundAtProvider)
            } else {
                Decision::Recheck {
                    not_found_checks: checks,
                }
            };
        }
        Err(error) => error,
    };
    if let Some(defer) = defer_if_blocked(&error, now) {
        return defer;
    }
    match error {
        // 다시 물어도 같은 답일 우리 쪽 버그. 관리자가 보게 한다.
        error @ ProviderError::Client { .. } => {
            Decision::GiveUp(format!("unresolved_unknown: {error}"))
        }
        // 조회는 업체 상태를 바꾸지 않으므로 몇 번을 다시 물어도 안전하다
        error => Decision::Retry(error.to_string()),
    }
}

pub(super) async fn handle(ctx: &WorkerContext, job: &Job) -> Result<(), WorkerError> {
    let lease = job.lease();
    let Some(booking) = super::load_booking(ctx, job).await? else {
        return Ok(());
    };
    if booking.status != BookingStatus::Unknown {
        tracing::info!(status = %booking.status, "이미 결과가 정해진 예약이라 작업만 끝낸다");
        return complete_only(ctx, lease).await;
    }
    let Some(provider) = ctx.providers.get(&booking.request.provider) else {
        let reason = format!("provider_not_configured: {}", booking.request.provider);
        return give_up(ctx, lease, &booking, reason).await;
    };
    if !provider.capabilities().lookup_by_reference {
        // 멱등키도 참조값 조회도 없는 업체: 자동으로 다시 제출하면 이중 예약 위험이 있어 관리자에게 넘긴다
        let reason = "unresolved_unknown: 업체가 참조값 조회를 지원하지 않는다".to_owned();
        return give_up(ctx, lease, &booking, reason).await;
    }

    // payload 형식이 깨졌으면 처음부터 다시 센다 (더 조심하는 쪽)
    let progress: Progress = serde_json::from_value(job.payload.clone()).unwrap_or_default();
    let call = CallContext {
        booking_id: Some(booking.id),
        job_id: Some(job.id),
    };
    let result = provider.find_booking(booking.id, call).await;
    match &result {
        Ok(found) => tracing::info!(
            found = found.is_some(),
            "결과 불명 예약을 업체에서 조회했다"
        ),
        Err(error) if error.blocked_until().is_some() => {}
        Err(error) => tracing::warn!(outcome = error.outcome(), %error, "업체 조회 실패"),
    }

    let decision = decide_resolution(result, progress.not_found_checks, Instant::now());
    apply(ctx, job, &booking, decision).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn 업체에_있으면_확정한다() {
        let found = Ok(Some(ProviderBooking {
            provider_booking_id: "rsv_1".into(),
        }));
        assert_eq!(
            decide_resolution(found, 0, now()),
            Decision::Transition(BookingEvent::FoundAtProvider {
                provider_booking_id: "rsv_1".into()
            })
        );
    }

    #[test]
    fn 없다는_것을_두_번_확인해야_다시_제출한다() {
        assert_eq!(
            decide_resolution(Ok(None), 0, now()),
            Decision::Recheck {
                not_found_checks: 1
            },
            "한 번 보고는 다시 제출하지 않는다"
        );
        assert_eq!(
            decide_resolution(Ok(None), 1, now()),
            Decision::Transition(BookingEvent::NotFoundAtProvider)
        );
    }

    #[test]
    fn 조회_실패는_다시_시도하고_우리_쪽에서_막히면_미룬다() {
        assert!(matches!(
            decide_resolution(Err(ProviderError::Timeout), 1, now()),
            Decision::Retry(_)
        ));
        let at = now();
        assert!(matches!(
            decide_resolution(
                Err(ProviderError::CircuitOpen {
                    retry_at: at + Duration::from_secs(5)
                }),
                0,
                at
            ),
            Decision::Defer { wait, .. } if wait == Duration::from_secs(5)
        ));
        let bug = ProviderError::Client {
            status: 400,
            body: String::new(),
        };
        assert!(matches!(
            decide_resolution(Err(bug), 0, now()),
            Decision::GiveUp(reason) if reason.starts_with("unresolved_unknown")
        ));
    }
}
