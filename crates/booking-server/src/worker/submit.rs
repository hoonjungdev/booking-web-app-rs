//! `submit_booking` 작업: 접수된 예약을 업체에 제출한다
//!
//! 관리자가 실패한 예약을 다시 시도하면 작업에 `{"lookup_first": true}`가 붙는다. 그러면 보내기 전에 업체에서
//! 이 예약을 먼저 찾아본다 (docs/03-resilience.md §5). 실패한 예약 중에는 업체에 실제로 만들어진 것이 있을 수 있어서
//! (결과 불명이었다가 조회도 실패해 포기한 경우 등) 그대로 다시 보내면 멱등키 없는 업체에서는 이중 예약이 된다.
//!
//! ```text
//! lookup_first ─┬─ 업체에 있음 ─────▶ confirmed (보내지 않음)
//!               ├─ 업체에 없음 ─────▶ 제출 (아래 decide)
//!               └─ 조회 실패 ───────▶ 작업 재예약 (표시는 그대로라 다음에도 조회부터 한다)
//! ```
//!
//! **멱등키 없는 업체에는 보내기 직전에 "보냄" 표시를 커밋한다** (Phase 6, docs/decisions.md D-31).
//! 보낸 뒤 결과를 기록하기 전에 워커가 죽으면(또는 그 기록이 DB 오류로 실패하면) lease가 끝난 뒤 다른 워커가 같은 작업을
//! 다시 가져간다. 그 워커는 표시를 보고 **다시 보내지 않고** 결과 불명(`unknown`)으로 넘겨 업체에서 조회부터 한다.
//!
//! ```text
//! 워커 A: [보냄 표시 커밋] ─▶ 업체에 POST ─▶ (죽음) ✗ 결과 기록 못 함
//! 워커 B: 다시 가져감 ─▶ 보냄 표시가 있다 ─▶ unknown + resolve_unknown (업체에서 참조값으로 조회)
//! ```

use std::time::Instant;

use chrono::Utc;
use db::jobs::{self, Job};
use domain::{BookingEvent, BookingStatus};
use providers::{CallContext, Capabilities, CreateBooking, ProviderBooking, ProviderError};
use serde::{Deserialize, Serialize};

use super::{
    Decision, WorkerContext, WorkerError, apply, complete_only, defer_if_blocked, give_up,
    warn_lease_lost,
};

/// 앞선 실행이 업체에 보낸 뒤 결과를 기록하지 못하고 멈췄을 때 남기는 결과 불명 사유
pub const UNRECORDED_SEND: &str =
    "unrecorded_send: 앞선 처리가 업체에 요청을 보낸 뒤 결과를 기록하지 못했다";

/// `submit_booking` 작업의 진행 상황 (`jobs.payload`)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitPayload {
    /// 보내기 전에 업체에서 이 예약을 먼저 찾아본다 (관리자 재시도)
    #[serde(default)]
    pub lookup_first: bool,
}

impl SubmitPayload {
    /// 작업의 payload를 읽는다. 형식이 깨졌으면 조회부터 한다 (더 조심하는 쪽).
    fn read(payload: &serde_json::Value) -> Self {
        serde_json::from_value(payload.clone()).unwrap_or(Self { lookup_first: true })
    }
}

/// 보내기 전에 먼저 조회한 결과 → 다음 행동. `None`이면 업체에 없으므로 제출을 이어서 한다 (순수 함수).
///
/// 관리자 재시도는 예약이 실패하고 한참 뒤(사람이 보고 누른 뒤)에 일어나므로, 결과 불명 확인(`resolve_unknown`)처럼
/// "없음"을 두 번 확인하지 않고 한 번으로 판단한다.
pub fn decide_lookup(
    result: Result<Option<ProviderBooking>, ProviderError>,
    now: Instant,
) -> Option<Decision> {
    let error = match result {
        Ok(Some(found)) => {
            return Some(Decision::Transition(BookingEvent::ProviderConfirmed {
                provider_booking_id: found.provider_booking_id,
            }));
        }
        Ok(None) => return None,
        Err(error) => error,
    };
    if let Some(defer) = defer_if_blocked(&error, now) {
        return Some(defer);
    }
    Some(match error {
        // 다시 물어도 같은 답일 우리 쪽 버그. 관리자가 보게 한다.
        error @ ProviderError::Client { .. } => Decision::GiveUp(format!("lookup_failed: {error}")),
        // 조회는 업체 상태를 바꾸지 않으므로 몇 번을 다시 물어도 안전하다
        error => Decision::Retry(format!("lookup_failed: {error}")),
    })
}

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
    if job.sent_at.is_some() {
        // 앞선 실행이 멱등키 없는 업체에 보낸 뒤 결과를 남기지 못했다. 업체에 예약이 생겼을 수 있으므로 다시 보내지 않는다.
        // 기한이 지났어도 먼저 결과 불명으로 보내 업체에서 확인한다 (그냥 포기하면 업체에만 예약이 남을 수 있다).
        tracing::warn!(
            "앞선 처리가 보낸 요청의 결과를 모른다. 다시 보내지 않고 결과 불명으로 넘긴다"
        );
        let unknown = BookingEvent::OutcomeUnknown {
            detail: UNRECORDED_SEND.to_owned(),
        };
        return apply(ctx, job, &booking, Decision::Transition(unknown)).await;
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
    if SubmitPayload::read(&job.payload).lookup_first {
        if provider.capabilities().lookup_by_reference {
            let found = provider.find_booking(booking.id, call).await;
            match &found {
                Ok(found) => tracing::info!(
                    found = found.is_some(),
                    "다시 보내기 전에 업체에서 예약을 찾아봤다"
                ),
                Err(error) if error.blocked_until().is_some() => {}
                Err(error) => tracing::warn!(outcome = error.outcome(), %error, "업체 조회 실패"),
            }
            if let Some(decision) = decide_lookup(found, Instant::now()) {
                return apply(ctx, job, &booking, decision).await;
            }
        } else {
            // 확인할 방법이 없다. 관리자가 업체에 직접 확인하고 재시도했다고 보고 보낸다.
            tracing::warn!("업체가 참조값 조회를 지원하지 않아 확인 없이 다시 보낸다");
        }
    }
    if !provider.capabilities().idempotency_key {
        // 멱등키가 없어 두 번 보내면 예약이 둘 생긴다. 보내기 직전에 "보냄" 표시를 커밋해 둔다 (모듈 설명).
        // 이때 lease도 새로 잡는다: 앞의 조회(lookup_first)에 lease를 거의 다 썼어도 제출 한 번이 끝날 시간이 남는다.
        // lease를 잃었으면 이미 다른 워커가 이 작업을 가져간 것이라 보내지 않는다.
        let mut conn = ctx.pool.acquire().await?;
        if !jobs::mark_sent(&mut conn, lease, ctx.config.lease).await? {
            warn_lease_lost();
            return Ok(());
        }
    }
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
    fn 먼저_조회해서_있으면_보내지_않고_확정한다() {
        let found = Ok(Some(ProviderBooking {
            provider_booking_id: "rsv_1".into(),
        }));
        assert_eq!(
            decide_lookup(found, Instant::now()),
            Some(Decision::Transition(BookingEvent::ProviderConfirmed {
                provider_booking_id: "rsv_1".into()
            }))
        );
        assert_eq!(
            decide_lookup(Ok(None), Instant::now()),
            None,
            "없으면 제출한다"
        );
    }

    #[test]
    fn 먼저_조회하다_실패하면_보내지_않고_다시_조회한다() {
        let now = Instant::now();
        assert!(matches!(
            decide_lookup(Err(ProviderError::Timeout), now),
            Some(Decision::Retry(_))
        ));
        let blocked = ProviderError::CircuitOpen {
            retry_at: now + Duration::from_secs(5),
        };
        assert!(matches!(
            decide_lookup(Err(blocked), now),
            Some(Decision::Defer { .. })
        ));
        let client = ProviderError::Client {
            status: 400,
            body: String::new(),
        };
        assert!(matches!(
            decide_lookup(Err(client), now),
            Some(Decision::GiveUp(_))
        ));
    }

    #[test]
    fn payload가_깨졌으면_조회부터_한다() {
        assert_eq!(
            SubmitPayload::read(&serde_json::json!({})),
            SubmitPayload::default()
        );
        assert!(SubmitPayload::read(&serde_json::json!({ "lookup_first": true })).lookup_first);
        assert!(SubmitPayload::read(&serde_json::json!("broken")).lookup_first);
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
