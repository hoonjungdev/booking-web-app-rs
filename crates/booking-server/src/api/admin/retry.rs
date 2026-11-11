//! 실패한 예약 재시도와 dead 작업 재큐잉 (관리자 조작)
//!
//! ```text
//! POST /bookings/{id}/retry ─┐
//!                            ├─▶ [한 트랜잭션] 예약 잠금 ─┬─ failed → pending (AdminRetry) → 그 예약의 dead 작업을
//! POST /jobs/{id}/requeue ───┘                            │    submit_booking + {"lookup_first": true}로 되살림 (없으면 새로 등록)
//!                                                         └─ cancel_failed → cancel_requested (AdminRetry, Phase 6) → dead 작업을
//!                                                              cancel_booking으로 되살림 (없으면 새로 등록)
//! ```
//!
//! 지키는 규칙
//! - **한 예약에 처리 중인 작업은 하나뿐**: 예약 행을 잠그고(`FOR UPDATE`), 대기 · 실행 중인 작업이 있으면 거절한다.
//!   작업이 둘이면 두 워커가 같은 예약을 동시에 업체에 보낼 수 있다 (멱등키 없는 업체에서는 이중 예약).
//! - **dead 작업은 새로 만들지 않고 되살린다**: 같은 행을 다시 대기열에 넣으므로 dead 작업 수가 바로 줄어든다.
//! - **다시 보내기 전에 업체에서 먼저 찾는다** (`lookup_first`): 실패한 예약 중에는 업체에 실제로 만들어진 것이 있을 수 있다
//!   (결과 불명이었다가 조회도 실패해 포기한 경우 등, docs/03-resilience.md §5). 워커가 이 표시를 보고 조회부터 한다.
//! - **확정 기한을 다시 잡는다**: 장애로 실패한 예약은 대개 기한(`deadline_at`)이 지나 포기한 것이다. 기한을 그대로 두면
//!   워커가 바로 다시 포기하므로, 재시도한 시각부터 새로 계산한다 (접수할 때와 같은 규칙: 30분 뒤 또는 슬롯 시작 중 이른 쪽).
//!   슬롯이 이미 시작했으면 재시도할 수 없다 (409).
//! - **시도 횟수는 되돌리지 않는다**: `attempts`는 lease 증표라 늘어나기만 해야 한다. 대신 한도를 다시 준다 (`jobs::requeue`).
//! - 상태 전이 · 작업 정리 · 감사 로그를 한 트랜잭션으로 저장한다 ("상태는 pending인데 처리할 작업이 없는" 유실을 막는다).
//! - 취소 실패(`cancel_failed`) 재시도는 다시 취소를 보낸다. 취소는 다시 보내도 결과가 같고(이미 취소된 예약도 성공),
//!   슬롯이 지났어도 업체가 판단하게 둔다 (확정 기한은 제출 흐름에만 있다).
//! - 포기한 대사 작업(`reconcile_provider`)은 되살리지 않는다. 업체마다 대사 작업은 하나만 대기 · 실행 중일 수 있는데
//!   (`jobs_reconcile_active_idx`), 대사는 포기할 때 다음 정기 대사를 함께 등록하므로 그 자리가 이미 차 있다.
//!   그래서 포기한 행은 끝난 것으로 정리하고, 그 업체의 대사를 지금 돌게 한다 ("지금 대사"와 같다).

use chrono::Utc;
use db::{
    PgPool, TraceRef,
    bookings::{self, Transitioned},
    jobs::{self, JobKind, JobRecord},
};
use domain::{Booking, BookingEvent, BookingId, BookingStatus, ProviderId, confirmation_deadline};
use sqlx::PgConnection;

use super::record_audit;
use crate::{api::ApiError, metrics, worker::SubmitPayload};

/// 재시도 · 재큐잉의 결과
#[derive(Debug, Clone)]
pub struct RetryOutcome {
    /// 다시 처리하게 된 예약 (조작 뒤의 상태). 예약과 상관없는 작업을 되살렸으면 `None`
    pub booking: Option<Booking>,
    /// 대기열에 들어간 작업 (조작 뒤의 상태)
    pub job: JobRecord,
}

/// 실패한(`failed`) 예약이나 취소에 실패한(`cancel_failed`) 예약을 다시 처리하게 한다.
///
/// `trace`는 이 조작을 일으킨 요청의 trace다. 상태 이력에 남기고, 작업에 실어 워커가 그 요청으로 link를 건다.
/// `actor`는 조작한 관리자(`admin:<이름>`)다. 상태 이력과 감사 로그에 남는다.
pub async fn retry_booking(
    pool: &PgPool,
    booking_id: BookingId,
    trace: &TraceRef,
    actor: &str,
) -> Result<RetryOutcome, ApiError> {
    let mut tx = pool.begin().await?;
    let booking = bookings::get_for_update(&mut tx, booking_id)
        .await?
        .ok_or(ApiError::BookingNotFound)?;
    let retried = retry_locked(&mut tx, &booking, None, trace, actor).await?;
    let detail = serde_json::json!({ "from": booking.status.as_str(), "job_id": retried.job_id });
    record_audit(
        &mut tx,
        actor,
        "booking.retry",
        Some(&booking.id.to_string()),
        detail,
    )
    .await?;
    tx.commit().await?;
    finished(pool, Some((&booking, retried.to)), retried.job_id).await
}

/// dead 작업을 다시 대기열에 넣는다.
///
/// 작업의 예약이 `failed`면(작업이 포기하면서 예약도 실패시킨 경우) 예약 재시도와 같다: 예약을 `pending`으로 되돌리고
/// 이 작업을 `submit_booking` + `lookup_first`로 되살린다. 그 밖에는 예약 상태는 그대로 두고 작업만 되살린다
/// (예약이 없어서 묻힌 작업, 이 서버가 모르는 종류라서 묻힌 작업 등). 이때도 `submit_booking`이면 다시 보내기 전에
/// 업체에서 먼저 찾게 하고, 예약이 아직 `pending`이면 확정 기한을 다시 잡는다 (앞선 시도가 업체까지 갔을 수 있고,
/// 지난 기한 그대로면 워커가 바로 다시 포기한다).
pub async fn requeue_job(
    pool: &PgPool,
    job_id: i64,
    trace: &TraceRef,
    actor: &str,
) -> Result<RetryOutcome, ApiError> {
    let mut tx = pool.begin().await?;
    let job = jobs::get(&mut tx, job_id)
        .await?
        .ok_or(ApiError::JobNotFound)?;
    if job.status != "dead" {
        return Err(ApiError::Conflict(format!(
            "dead 작업만 다시 넣을 수 있습니다 (지금: {})",
            job.status
        )));
    }

    let booking = match job.booking_id {
        Some(booking_id) => bookings::get_for_update(&mut tx, booking_id).await?,
        None => None,
    };
    // 실패한 예약이면 예약 재시도 (취소 실패도). 준비한 작업의 ID를 받는다 (보통 이 작업이다)
    let retried = match &booking {
        Some(booking)
            if matches!(
                booking.status,
                BookingStatus::Failed | BookingStatus::CancelFailed
            ) =>
        {
            Some(retry_locked(&mut tx, booking, Some(job.id), trace, actor).await?)
        }
        _ => None,
    };
    let job_id = match retried {
        Some(ref retried) => retried.job_id,
        None if job.kind == JobKind::ReconcileProvider.as_str() => {
            reconcile_now_instead(&mut tx, &job, trace).await?
        }
        None => {
            if let Some(booking) = &booking {
                ensure_no_active_job(&mut tx, booking.id).await?;
            }
            let mut payload = job.payload.clone();
            if job.kind == JobKind::SubmitBooking.as_str() {
                payload = lookup_first_payload()?;
                if let Some(booking) = &booking
                    && booking.status == BookingStatus::Pending
                {
                    renew_deadline(&mut tx, booking).await?;
                }
            }
            let requeued =
                jobs::requeue(&mut tx, job.id, &job.kind, &payload, trace.context.as_ref()).await?;
            if !requeued {
                return Err(ApiError::Conflict(
                    "그사이 다른 곳에서 이 작업을 되살렸습니다".into(),
                ));
            }
            job.id
        }
    };
    let detail = serde_json::json!({ "kind": job.kind, "retried_booking": retried.is_some() });
    record_audit(
        &mut tx,
        actor,
        "job.requeue",
        Some(&job.id.to_string()),
        detail,
    )
    .await?;
    tx.commit().await?;
    let retried_booking = booking
        .as_ref()
        .zip(retried.as_ref())
        .map(|(booking, retried)| (booking, retried.to));
    tracing::info!(
        job_id,
        kind = %job.kind,
        retried = retried.is_some(),
        %actor,
        "관리자가 dead 작업을 다시 넣었다"
    );
    finished(pool, retried_booking, job_id).await
}

/// 포기한 대사 작업을 다시 넣는 대신: 그 행은 끝난 것으로 정리하고, 그 업체의 대사를 지금 돌게 한다.
/// 돌게 될 대사 작업(대기 중이던 다음 정기 대사, 없으면 새로 등록한 것)의 ID를 돌려준다.
async fn reconcile_now_instead(
    tx: &mut PgConnection,
    job: &JobRecord,
    trace: &TraceRef,
) -> Result<i64, ApiError> {
    let provider = job
        .provider_id
        .as_deref()
        .and_then(|id| ProviderId::parse(id).ok())
        .ok_or_else(|| {
            ApiError::Conflict("업체가 적혀 있지 않은 대사 작업은 다시 넣을 수 없습니다".into())
        })?;
    let note = "관리자가 다시 넣어 그 업체의 대사를 지금 돌리는 것으로 대신했다";
    if !jobs::close_dead(tx, job.id, note).await? {
        return Err(ApiError::Conflict(
            "그사이 다른 곳에서 이 작업을 정리했습니다".into(),
        ));
    }
    let active = jobs::run_reconcile_now(tx, &provider, trace.context.as_ref()).await?;
    Ok(active.id)
}

/// 예약 재시도로 준비한 것: 처리할 작업과 예약이 바뀐 상태
struct Retried {
    job_id: i64,
    to: BookingStatus,
}

/// 잠근 예약을 다시 처리하게 하고 처리할 작업을 준비한다.
///
/// - `failed` → `pending`: 확정 기한을 다시 잡고, `submit_booking` + `lookup_first`로 (업체에서 먼저 찾아본 뒤 제출)
/// - `cancel_failed` → `cancel_requested`: `cancel_booking`으로 (취소를 다시 보낸다)
///
/// `job`: 되살릴 dead 작업. `None`이면 그 예약의 가장 최근 dead 작업을 되살리고, 그것도 없으면 새로 등록한다.
async fn retry_locked(
    tx: &mut PgConnection,
    booking: &Booking,
    job: Option<i64>,
    trace: &TraceRef,
    actor: &str,
) -> Result<Retried, ApiError> {
    let (kind, payload) = match booking.status {
        BookingStatus::Failed => (JobKind::SubmitBooking, lookup_first_payload()?),
        BookingStatus::CancelFailed => (JobKind::CancelBooking, serde_json::json!({})),
        other => {
            return Err(ApiError::Conflict(format!(
                "failed · cancel_failed 상태인 예약만 재시도할 수 있습니다 (지금: {other})"
            )));
        }
    };
    ensure_no_active_job(tx, booking.id).await?;
    if booking.status == BookingStatus::Failed {
        // 기한을 지금부터 다시 잡는다 (지난 기한 그대로면 워커가 바로 다시 포기한다). 슬롯이 이미 시작했으면 409.
        renew_deadline(tx, booking).await?;
    }

    let transitioned = bookings::transition(
        tx,
        booking.id,
        booking.status,
        &BookingEvent::AdminRetry,
        actor,
        trace.trace_id.as_deref(),
    )
    .await?;
    let Transitioned::Applied(to) = transitioned else {
        // 예약을 잠갔으므로 일어나지 않는다. 그래도 일어나면 아무것도 바꾸지 않고 알린다 (트랜잭션은 커밋되지 않는다).
        return Err(ApiError::Conflict("그사이 예약 상태가 바뀌었습니다".into()));
    };

    let dead = match job {
        Some(id) => Some(id),
        None => jobs::latest_dead_for_booking(tx, booking.id).await?,
    };
    if let Some(id) = dead
        && jobs::requeue(tx, id, kind.as_str(), &payload, trace.context.as_ref()).await?
    {
        return Ok(Retried { job_id: id, to });
    }
    let job_id = jobs::enqueue_with_payload(
        tx,
        kind,
        Some(booking.id),
        jobs::DEFAULT_MAX_ATTEMPTS,
        &payload,
        trace.context.as_ref(),
    )
    .await?;
    Ok(Retried { job_id, to })
}

/// 다시 처리할 예약의 확정 기한을 지금부터 다시 잡는다 (접수와 같은 규칙: 30분 뒤 또는 슬롯 시작 중 이른 쪽).
/// 슬롯이 이미 시작했으면 다시 처리할 수 없다 (409).
async fn renew_deadline(tx: &mut PgConnection, booking: &Booking) -> Result<(), ApiError> {
    let now = db::normalize_timestamp(Utc::now());
    if booking.request.slot_start <= now {
        return Err(ApiError::Conflict(
            "슬롯이 이미 시작해서 다시 처리할 수 없습니다".into(),
        ));
    }
    let deadline = confirmation_deadline(now, booking.request.slot_start);
    bookings::set_deadline(tx, booking.id, deadline).await?;
    Ok(())
}

/// "다시 보내기 전에 업체에서 먼저 찾아보라"는 `submit_booking` 작업의 진행 상황
fn lookup_first_payload() -> Result<serde_json::Value, ApiError> {
    serde_json::to_value(SubmitPayload { lookup_first: true })
        .map_err(|error| ApiError::Internal(error.to_string()))
}

/// 예약에 대기 · 실행 중인 작업이 있으면 거절한다 (한 예약에 처리 중인 작업은 하나뿐)
async fn ensure_no_active_job(
    tx: &mut PgConnection,
    booking_id: BookingId,
) -> Result<(), ApiError> {
    if jobs::has_active_for_booking(tx, booking_id).await? {
        return Err(ApiError::Conflict(
            "이 예약에는 대기 중이거나 실행 중인 작업이 있습니다".into(),
        ));
    }
    Ok(())
}

/// 커밋한 뒤: 지표를 세고, 바뀐 예약과 작업을 다시 읽어 돌려준다.
/// `retried`는 되돌린 예약(바뀌기 전 값)과 바뀐 상태다.
async fn finished(
    pool: &PgPool,
    retried: Option<(&Booking, BookingStatus)>,
    job_id: i64,
) -> Result<RetryOutcome, ApiError> {
    let booking = match retried {
        Some((before, to)) => {
            // 지표는 커밋한 뒤에 센다 (커밋이 실패하면 전이는 일어나지 않은 것이다)
            metrics::booking_transitioned(before, to);
            tracing::info!(booking_id = %before.id, job_id, from = %before.status, "관리자가 예약을 다시 처리한다");
            bookings::get(pool, before.id).await?
        }
        None => None,
    };
    let mut conn = pool.acquire().await?;
    let job = jobs::get(&mut conn, job_id)
        .await?
        .ok_or(ApiError::JobNotFound)?;
    Ok(RetryOutcome { booking, job })
}
