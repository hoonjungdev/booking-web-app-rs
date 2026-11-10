//! `submit_booking` 작업: 접수된 예약을 업체에 제출한다

use chrono::Utc;
use db::{
    bookings::{self, Transitioned},
    jobs::{self, Job, Lease},
};
use domain::{Booking, BookingEvent, BookingStatus};
use providers::{Capabilities, CreateBooking, ProviderBooking, ProviderError};
use rand::RngExt;

use super::{ACTOR, WorkerContext, WorkerError, job_backoff};

/// 업체 호출 결과를 보고 정한 다음 행동
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 이 사건으로 예약 상태를 바꾸고 작업을 끝낸다
    Transition(BookingEvent),
    /// 나중에 다시 시도한다 (사유)
    Retry(String),
    /// 더 시도하지 않고 포기한다 (사유)
    GiveUp(String),
}

/// 업체 호출 결과 → 다음 행동. DB도 네트워크도 쓰지 않는 순수 함수라서 단위 테스트로 모든 경우를 확인한다.
///
/// 이중 예약을 막는 핵심 규칙 (docs/03-resilience.md §5):
/// 업체가 처리했는지 알 수 없는 실패(타임아웃 · 5xx · 전송 오류 · 응답 해석 실패)는
/// **멱등키를 지원하는 업체에만** 다시 보낸다. 지원하지 않으면 다시 보내지 않고 결과 불명(`unknown`)으로 둔다.
pub fn decide(
    result: Result<ProviderBooking, ProviderError>,
    capabilities: Capabilities,
) -> Decision {
    match result {
        Ok(booking) => Decision::Transition(BookingEvent::ProviderConfirmed {
            provider_booking_id: booking.provider_booking_id,
        }),
        Err(ProviderError::Rejected { reason }) => {
            Decision::Transition(BookingEvent::ProviderRejected { reason })
        }
        // 예상하지 못한 4xx는 다시 보내도 같은 결과라서(대개 우리 버그) 포기하고 관리자가 보게 한다
        Err(error @ ProviderError::Client { .. }) => Decision::GiveUp(error.to_string()),
        Err(error) if error.definitely_not_processed() || capabilities.idempotency_key => {
            Decision::Retry(error.to_string())
        }
        Err(error) => Decision::Transition(BookingEvent::OutcomeUnknown {
            detail: error.to_string(),
        }),
    }
}

pub(super) async fn handle(ctx: &WorkerContext, job: &Job) -> Result<(), WorkerError> {
    let lease = job.lease();
    let Some(booking_id) = job.booking_id else {
        return bury(ctx, lease, "submit_booking 작업에 booking_id가 없습니다").await;
    };
    tracing::Span::current().record("booking_id", tracing::field::display(booking_id));

    let Some(booking) = bookings::get(&ctx.pool, booking_id).await? else {
        return bury(ctx, lease, "작업의 예약을 찾을 수 없습니다").await;
    };
    if booking.status != BookingStatus::Pending {
        // 다른 경로로 이미 처리됨 (lease 만료로 다른 워커가 먼저 끝낸 경우 등). 작업만 끝낸다.
        tracing::info!(status = %booking.status, "이미 처리된 예약이라 작업만 끝낸다");
        let mut conn = ctx.pool.acquire().await?;
        jobs::complete(&mut conn, lease).await?;
        return Ok(());
    }
    if Utc::now() >= booking.deadline_at {
        return give_up(ctx, lease, &booking, "deadline_exceeded".to_owned()).await;
    }
    let Some(adapter) = ctx.providers.get(&booking.request.provider) else {
        let reason = format!("provider_not_configured: {}", booking.request.provider);
        return give_up(ctx, lease, &booking, reason).await;
    };

    let result = adapter
        .create_booking(&CreateBooking::from_booking(&booking))
        .await;
    if let Err(error) = &result {
        tracing::warn!(outcome = error.outcome(), %error, "업체 호출 실패");
    }

    match decide(result, adapter.capabilities()) {
        Decision::Transition(event) => finish(ctx, lease, &booking, &event).await,
        Decision::Retry(reason) if job.attempts >= job.max_attempts => {
            give_up(
                ctx,
                lease,
                &booking,
                format!("attempts_exhausted: {reason}"),
            )
            .await
        }
        Decision::Retry(reason) => {
            let delay = job_backoff(job.attempts, rand::rng().random());
            tracing::info!(?delay, %reason, "나중에 다시 시도한다");
            let mut conn = ctx.pool.acquire().await?;
            if !jobs::retry_later(&mut conn, lease, delay, &reason).await? {
                warn_lease_lost();
            }
            Ok(())
        }
        Decision::GiveUp(reason) => give_up(ctx, lease, &booking, reason).await,
    }
}

/// 예약 상태 전이와 작업 완료를 한 트랜잭션으로 저장한다.
/// 둘 중 하나만 저장되면 "확정됐는데 작업이 또 도는" 식의 불일치가 생기기 때문이다.
async fn finish(
    ctx: &WorkerContext,
    lease: Lease,
    booking: &Booking,
    event: &BookingEvent,
) -> Result<(), WorkerError> {
    let mut tx = ctx.pool.begin().await?;
    if !jobs::complete(&mut tx, lease).await? {
        warn_lease_lost();
        return Ok(()); // tx가 commit 없이 버려지면서 롤백된다
    }
    match bookings::transition(&mut tx, booking.id, booking.status, event, ACTOR).await? {
        Transitioned::Applied(to) => tracing::info!(status = %to, "예약 상태를 바꿨다"),
        Transitioned::Conflict => tracing::warn!("그사이 예약 상태가 바뀌어 전이를 건너뛴다"),
    }
    tx.commit().await?;
    Ok(())
}

/// 예약을 `failed`로, 작업을 `dead`로 만든다 (한 트랜잭션)
async fn give_up(
    ctx: &WorkerContext,
    lease: Lease,
    booking: &Booking,
    reason: String,
) -> Result<(), WorkerError> {
    tracing::warn!(%reason, "예약 처리를 포기한다");
    let mut tx = ctx.pool.begin().await?;
    if !jobs::mark_dead(&mut tx, lease, &reason).await? {
        warn_lease_lost();
        return Ok(());
    }
    let event = BookingEvent::GaveUp { reason };
    bookings::transition(&mut tx, booking.id, booking.status, &event, ACTOR).await?;
    tx.commit().await?;
    Ok(())
}

/// 예약이 없는 등 작업 자체가 잘못된 경우: 작업만 `dead`로 남긴다
async fn bury(ctx: &WorkerContext, lease: Lease, reason: &str) -> Result<(), WorkerError> {
    tracing::error!(%reason, "처리할 수 없는 작업");
    let mut conn = ctx.pool.acquire().await?;
    jobs::mark_dead(&mut conn, lease, reason).await?;
    Ok(())
}

fn warn_lease_lost() {
    tracing::warn!("lease를 잃어 결과를 반영하지 않는다 (다른 워커가 이 작업을 다시 가져감)");
}

#[cfg(test)]
mod tests {
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
        let ok = decide(
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

        let rejected = decide(
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
                matches!(decide(Err(error.clone()), IDEMPOTENT), Decision::Retry(_)),
                "{error:?} + 멱등키 지원 → 재시도"
            );
            assert!(
                matches!(
                    decide(Err(error.clone()), NOT_IDEMPOTENT),
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
                decide(Err(error), NOT_IDEMPOTENT),
                Decision::Retry(_)
            ));
        }
    }

    #[test]
    fn 예상하지_못한_4xx는_포기한다() {
        let error = ProviderError::Client {
            status: 422,
            body: "{}".into(),
        };
        assert!(matches!(
            decide(Err(error), IDEMPOTENT),
            Decision::GiveUp(_)
        ));
    }
}
