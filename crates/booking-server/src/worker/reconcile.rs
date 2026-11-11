//! `reconcile_provider` 작업: 업체 예약 목록과 우리 예약을 비교한다 (대사, docs/03-resilience.md §9)
//!
//! ```text
//! 업체마다 하나 (끝날 때 다음 실행을 등록하는 작업 체인, 기본 10분 간격)
//! ├─ 우리 limiter 토큰이 절반 넘게 남아 있을 때만 업체를 부른다 (사용자가 기다리는 호출에 양보, §6)
//! ├─ 지난 24시간 ~ 앞으로 7일 슬롯의 업체 목록과 우리 예약을 비교한다 (compare: 순수 함수)
//! └─ 찾은 것마다 한 트랜잭션: 예약 상태 전이(자동으로 바로잡을 수 있는 것) + 이슈 기록
//! ```
//!
//! | 찾은 것 | 처리 |
//! |---|---|
//! | 우리는 `confirmed`, 업체에서는 취소됨 | 예약 → `cancelled` (actor `reconciler`), 이슈(처리 완료로) |
//! | 우리는 `failed` · `unknown`, 업체에서는 확정 | 예약 → `confirmed`, 이슈(처리 완료로). 실패 때 묻힌 dead 작업은 끝난 것으로 정리 |
//! | 우리는 `confirmed`, 업체에는 없음 | 이슈만 (자동으로 처리하지 않음) |
//! | 같은 참조값으로 확정된 업체 예약이 2건 이상 | 이슈만 (이중 예약 의심, 긴급 알림) |
//! | 업체에 우리 참조값 예약이 있는데 우리에게는 없음 | 이슈만 |
//! | 그 밖에 양쪽 상태가 다름 (우리는 취소 · 거절인데 업체는 확정 등) | 이슈만 |
//!
//! 처리 중인 예약(`pending`, `cancel_requested`)은 다른 작업이 다루고 있으므로 건너뛴다 (이중 예약 검사만 한다).

use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};

use chrono::{TimeDelta, Utc};
use db::{
    TraceRef,
    bookings::{self, Transitioned},
    jobs::{self, Job, Lease},
    reconciliation::{self, IssueKind, NewIssue},
};
use domain::{Booking, BookingEvent, BookingId, BookingStatus, ProviderId};
use providers::{CallContext, ListedBooking, ListedStatus, ProviderError};
use rand::RngExt;
use serde_json::json;
use uuid::Uuid;

use super::{
    Decision, WorkerContext, WorkerError, defer_if_blocked, record_decision, warn_lease_lost,
};
use crate::{metrics, trace};

/// 비교할 슬롯 범위: 지금보다 이만큼 앞 (지난 24시간)
pub const LOOKBACK: TimeDelta = TimeDelta::hours(24);
/// 비교할 슬롯 범위: 지금부터 이만큼 뒤까지 (앞으로 7일. 예약은 대개 앞으로의 슬롯에 있다)
pub const LOOKAHEAD: TimeDelta = TimeDelta::days(7);
/// `booking_events.actor`와 자동 처리한 이슈의 `resolved_by`
const RECONCILER: &str = "reconciler";
/// 우리 limiter에 여유가 없을 때 대사를 미루는 시간
const SPARE_CAPACITY_WAIT: Duration = Duration::from_secs(5);

/// 대사에서 찾은 것 (DB 없이 [`compare`]가 만든다)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// 우리는 확정인데 업체에서는 취소됨 → 예약을 취소로 바꾼다
    CancelledAtProvider {
        booking_id: BookingId,
        provider_booking_id: String,
    },
    /// 우리는 실패 · 결과 불명인데 업체에서는 확정 → 예약을 확정으로 바꾼다
    ConfirmedAtProvider {
        booking_id: BookingId,
        from: BookingStatus,
        provider_booking_id: String,
    },
    /// 우리는 확정인데 업체에는 그 예약이 없다
    MissingAtProvider {
        booking_id: BookingId,
        provider_booking_id: Option<String>,
    },
    /// 같은 참조값으로 확정된 업체 예약이 2건 이상 (이중 예약 의심)
    Duplicate {
        booking_id: BookingId,
        provider_booking_ids: Vec<String>,
    },
    /// 그 밖에 양쪽 상태가 다르다
    Mismatch {
        booking_id: BookingId,
        ours: BookingStatus,
        provider_booking_id: String,
        theirs: ListedStatus,
        note: &'static str,
    },
    /// 업체에 우리 참조값(예약 ID 형식)을 단 확정 예약이 있는데, 비교한 우리 예약 중에는 없다.
    /// 범위 밖에 있을 수도 있으므로 처리할 때 DB에서 한 번 더 찾아보고, 정말 없으면 이슈로 남긴다.
    UnknownReference {
        reference: BookingId,
        provider_booking_id: String,
    },
}

/// 우리 예약(`ours`)과 업체 목록(`theirs`)을 비교한다. DB도 네트워크도 쓰지 않는 순수 함수다.
pub fn compare(ours: &[Booking], theirs: &[ListedBooking]) -> Vec<Finding> {
    // 업체 예약을 참조값(우리 예약 ID)별로 묶는다. 예약 ID 형식이 아닌 참조값은 다른 경로로 만든 예약이라 뺀다.
    // BTreeMap: 키 순서대로 정렬되는 맵이라 결과 순서가 항상 같다 (테스트하기 쉽다)
    let mut by_reference: BTreeMap<BookingId, Vec<&ListedBooking>> = BTreeMap::new();
    for listed in theirs {
        let reference = listed
            .reference
            .as_deref()
            .and_then(|r| Uuid::parse_str(r).ok())
            .map(BookingId::from_uuid);
        if let Some(reference) = reference {
            by_reference.entry(reference).or_default().push(listed);
        }
    }

    let mut findings = Vec::new();
    for booking in ours {
        let entries = by_reference
            .get(&booking.id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let confirmed: Vec<&ListedBooking> = entries
            .iter()
            .copied()
            .filter(|e| e.status == ListedStatus::Confirmed)
            .collect();
        if confirmed.len() >= 2 {
            findings.push(Finding::Duplicate {
                booking_id: booking.id,
                provider_booking_ids: confirmed
                    .iter()
                    .map(|e| e.provider_booking_id.clone())
                    .collect(),
            });
        }
        if let Some(finding) = compare_one(booking, entries, &confirmed) {
            findings.push(finding);
        }
    }

    for (reference, entries) in &by_reference {
        if ours.iter().any(|b| b.id == *reference) {
            continue;
        }
        let confirmed: Vec<_> = entries
            .iter()
            .filter(|e| e.status == ListedStatus::Confirmed)
            .collect();
        if confirmed.len() >= 2 {
            findings.push(Finding::Duplicate {
                booking_id: *reference,
                provider_booking_ids: confirmed
                    .iter()
                    .map(|e| e.provider_booking_id.clone())
                    .collect(),
            });
        }
        for entry in confirmed {
            findings.push(Finding::UnknownReference {
                reference: *reference,
                provider_booking_id: entry.provider_booking_id.clone(),
            });
        }
    }
    findings
}

/// 우리 예약 하나와 같은 참조값의 업체 예약들(`entries`, 그중 확정된 것 `confirmed`)을 비교한다
fn compare_one(
    booking: &Booking,
    entries: &[&ListedBooking],
    confirmed: &[&ListedBooking],
) -> Option<Finding> {
    let first_confirmed = confirmed.first().map(|e| e.provider_booking_id.clone());
    match booking.status {
        BookingStatus::Confirmed => {
            // 우리가 아는 업체 예약 ID로 찾는다
            let ours = booking.provider_booking_id.as_deref();
            let mine = ours.and_then(|id| entries.iter().find(|e| e.provider_booking_id == id));
            match (mine, first_confirmed) {
                (Some(e), _) if e.status == ListedStatus::Confirmed => None,
                (Some(e), None) => Some(Finding::CancelledAtProvider {
                    booking_id: booking.id,
                    provider_booking_id: e.provider_booking_id.clone(),
                }),
                (Some(e), Some(_)) => Some(Finding::Mismatch {
                    booking_id: booking.id,
                    ours: booking.status,
                    provider_booking_id: e.provider_booking_id.clone(),
                    theirs: e.status,
                    note: "우리가 아는 업체 예약은 취소됐고 같은 참조값의 다른 예약이 확정되어 있다",
                }),
                (None, Some(other)) => Some(Finding::Mismatch {
                    booking_id: booking.id,
                    ours: booking.status,
                    provider_booking_id: other,
                    theirs: ListedStatus::Confirmed,
                    note: "업체에 확정된 예약은 있지만 우리가 아는 업체 예약 ID와 다르다",
                }),
                (None, None) => Some(Finding::MissingAtProvider {
                    booking_id: booking.id,
                    provider_booking_id: ours.map(str::to_owned),
                }),
            }
        }
        BookingStatus::Failed | BookingStatus::Unknown => {
            first_confirmed.map(|provider_booking_id| Finding::ConfirmedAtProvider {
                booking_id: booking.id,
                from: booking.status,
                provider_booking_id,
            })
        }
        BookingStatus::Cancelled | BookingStatus::Rejected => {
            first_confirmed.map(|provider_booking_id| Finding::Mismatch {
                booking_id: booking.id,
                ours: booking.status,
                provider_booking_id,
                theirs: ListedStatus::Confirmed,
                note: "우리는 끝난 예약인데 업체에서는 확정되어 있다",
            })
        }
        BookingStatus::CancelFailed => {
            // 취소에 실패해 관리자를 기다리는데, 업체에서는 이미 취소되어 있다 (관리자가 취소를 다시 시도하면 끝난다)
            let ours = booking.provider_booking_id.as_deref();
            entries
                .iter()
                .find(|e| Some(e.provider_booking_id.as_str()) == ours)
                .filter(|e| e.status == ListedStatus::Cancelled)
                .map(|e| Finding::Mismatch {
                    booking_id: booking.id,
                    ours: booking.status,
                    provider_booking_id: e.provider_booking_id.clone(),
                    theirs: ListedStatus::Cancelled,
                    note: "취소 실패로 남아 있지만 업체에서는 취소되어 있다 (취소를 다시 시도하면 끝난다)",
                })
        }
        // 다른 작업이 처리 중이다
        BookingStatus::Pending | BookingStatus::CancelRequested => None,
    }
}

pub(super) async fn handle(ctx: &WorkerContext, job: &Job) -> Result<(), WorkerError> {
    let lease = job.lease();
    let Some(provider_id) = job
        .provider_id
        .as_deref()
        .and_then(|id| ProviderId::parse(id).ok())
    else {
        return super::bury(ctx, lease, "대사 작업에 업체가 없습니다").await;
    };
    let Some(provider) = ctx.providers.get(&provider_id) else {
        // 이 프로세스에 그 업체의 파이프라인이 없다 (사용하지 않는 업체 등). 다음 실행을 등록하지 않고 끝낸다.
        tracing::info!(provider = %provider_id, "업체 파이프라인이 없어 대사를 하지 않는다");
        return super::complete_only(ctx, lease).await;
    };
    if !provider.capabilities().list_bookings {
        tracing::info!(provider = %provider_id, "업체가 기간별 목록을 지원하지 않아 대사를 하지 않는다");
        return super::complete_only(ctx, lease).await;
    }
    if !provider.has_spare_capacity() {
        // 사용자가 기다리는 호출(예약 · 빈 시간 조회)이 먼저다. 시도 횟수를 쓰지 않고 미룬다.
        return defer(
            ctx,
            lease,
            SPARE_CAPACITY_WAIT,
            "업체 호출 한도 여유가 절반 이하라 대사를 미룬다",
        )
        .await;
    }

    let now = Utc::now();
    let (from, to) = (now - LOOKBACK, now + LOOKAHEAD);
    let call = CallContext {
        booking_id: None,
        job_id: Some(job.id),
    };
    let theirs = match provider.list_bookings(from, to, call).await {
        Ok(listed) => listed,
        Err(error) => return failed_call(ctx, job, &provider_id, &error).await,
    };
    let ours = bookings::for_reconciliation(&ctx.pool, &provider_id, from, to).await?;
    let findings = compare(&ours, &theirs);
    tracing::info!(
        provider = %provider_id,
        ours = ours.len(),
        theirs = theirs.len(),
        findings = findings.len(),
        "대사를 마쳤다"
    );

    // 이 작업의 trace. 상태 이력에 남기고, 다음 대사 작업이 이 trace로 link를 건다.
    let trace = trace::current();
    let by_id: HashMap<BookingId, &Booking> = ours.iter().map(|b| (b.id, b)).collect();
    for finding in findings {
        apply_finding(ctx, &provider_id, finding, &by_id, &trace).await?;
    }
    end_run(ctx, lease, &provider_id, None, &trace).await
}

/// 찾은 것 하나를 처리한다: 자동으로 바로잡을 수 있으면 상태 전이 + 이슈(처리 완료)를, 아니면 이슈만 기록한다
async fn apply_finding(
    ctx: &WorkerContext,
    provider: &ProviderId,
    finding: Finding,
    bookings: &HashMap<BookingId, &Booking>,
    trace: &TraceRef,
) -> Result<(), WorkerError> {
    let issue =
        |kind, booking_id: BookingId, provider_booking_id: Option<String>, detail| NewIssue {
            provider_id: provider.clone(),
            booking_id: Some(booking_id),
            provider_booking_id,
            kind,
            detail,
            resolved_by: None,
        };
    match finding {
        Finding::CancelledAtProvider {
            booking_id,
            provider_booking_id,
        } => {
            let Some(booking) = bookings.get(&booking_id) else {
                return Ok(());
            };
            tracing::warn!(%booking_id, "업체에서 취소된 예약을 찾아 취소로 바꾼다");
            let fixed = NewIssue {
                resolved_by: Some(RECONCILER.to_owned()),
                ..issue(
                    IssueKind::StatusMismatch,
                    booking_id,
                    Some(provider_booking_id),
                    json!({ "ours": "confirmed", "theirs": "cancelled", "action": "cancelled",
                            "note": "업체 쪽에서 취소된 예약" }),
                )
            };
            fix(
                ctx,
                booking,
                &BookingEvent::CancelledExternally,
                &fixed,
                trace,
            )
            .await
        }
        Finding::ConfirmedAtProvider {
            booking_id,
            from,
            provider_booking_id,
        } => {
            let Some(booking) = bookings.get(&booking_id) else {
                return Ok(());
            };
            tracing::warn!(%booking_id, from = %from, "업체에서 확정된 예약을 찾아 확정으로 바꾼다");
            let fixed = NewIssue {
                resolved_by: Some(RECONCILER.to_owned()),
                ..issue(
                    IssueKind::StatusMismatch,
                    booking_id,
                    Some(provider_booking_id.clone()),
                    json!({ "ours": from.as_str(), "theirs": "confirmed", "action": "confirmed" }),
                )
            };
            let event = BookingEvent::FoundAtProvider {
                provider_booking_id,
            };
            fix(ctx, booking, &event, &fixed, trace).await
        }
        Finding::MissingAtProvider {
            booking_id,
            provider_booking_id,
        } => {
            tracing::warn!(%booking_id, "확정된 예약이 업체에 없다");
            let detail = json!({ "ours": "confirmed", "theirs": null });
            let new = issue(
                IssueKind::MissingAtProvider,
                booking_id,
                provider_booking_id,
                detail,
            );
            record_issue(ctx, &new).await
        }
        Finding::Duplicate {
            booking_id,
            provider_booking_ids,
        } => {
            // 긴급: 업체에 같은 예약이 두 번 있다. 알림(reconciliation.issues.open{kind="duplicate_suspected"})이 울린다.
            tracing::error!(%booking_id, count = provider_booking_ids.len(), "이중 예약 의심: 같은 참조값으로 확정된 업체 예약이 여러 건이다");
            let detail = json!({ "provider_booking_ids": provider_booking_ids });
            let new = issue(IssueKind::DuplicateSuspected, booking_id, None, detail);
            record_issue(ctx, &new).await
        }
        Finding::Mismatch {
            booking_id,
            ours,
            provider_booking_id,
            theirs,
            note,
        } => {
            tracing::warn!(%booking_id, ours = %ours, note, "우리와 업체의 예약 상태가 다르다");
            let detail =
                json!({ "ours": ours.as_str(), "theirs": listed_status(theirs), "note": note });
            let new = issue(
                IssueKind::StatusMismatch,
                booking_id,
                Some(provider_booking_id),
                detail,
            );
            record_issue(ctx, &new).await
        }
        Finding::UnknownReference {
            reference,
            provider_booking_id,
        } => {
            // 비교 범위 밖에 있는 우리 예약일 수 있다. 정말 없을 때만 이슈로 남긴다.
            if bookings::get(&ctx.pool, reference).await?.is_some() {
                return Ok(());
            }
            tracing::warn!(%reference, "업체에 우리 참조값 예약이 있는데 우리에게는 없다");
            let detail = json!({ "ours": null, "theirs": "confirmed" });
            let new = issue(
                IssueKind::MissingLocally,
                reference,
                Some(provider_booking_id),
                detail,
            );
            record_issue(ctx, &new).await
        }
    }
}

/// 예약 상태를 바로잡고 이슈(처리 완료)를 남긴다. 한 트랜잭션이다.
/// 그사이 다른 곳에서 상태가 바뀌었으면(낙관적 동시성 충돌) 아무것도 하지 않는다. 다음 대사가 다시 본다.
async fn fix(
    ctx: &WorkerContext,
    booking: &Booking,
    event: &BookingEvent,
    issue: &NewIssue,
    trace: &TraceRef,
) -> Result<(), WorkerError> {
    let mut tx = ctx.pool.begin().await?;
    let transitioned = bookings::transition(
        &mut tx,
        booking.id,
        booking.status,
        event,
        RECONCILER,
        trace.trace_id.as_deref(),
    )
    .await?;
    let Transitioned::Applied(to) = transitioned else {
        tracing::info!(booking_id = %booking.id, "그사이 예약 상태가 바뀌어 다음 대사에서 다시 본다");
        return Ok(()); // tx가 commit 없이 버려지면서 롤백된다
    };
    if booking.status == BookingStatus::Failed {
        // 실패할 때 묻은 작업은 이제 할 일이 없다. dead로 남겨 두면 "dead 작업이 있음" 알림이 계속 울린다.
        jobs::close_dead_for_booking(&mut tx, booking.id, "대사가 업체에서 확정된 예약을 찾았다")
            .await?;
    }
    reconciliation::record(&mut tx, issue).await?;
    tx.commit().await?;
    // 지표는 커밋한 뒤에 센다
    metrics::booking_transitioned(booking, to);
    Ok(())
}

async fn record_issue(ctx: &WorkerContext, issue: &NewIssue) -> Result<(), WorkerError> {
    let mut conn = ctx.pool.acquire().await?;
    if reconciliation::record(&mut conn, issue).await?.is_none() {
        tracing::debug!(kind = issue.kind.as_str(), "같은 미해결 이슈가 이미 있다");
    }
    Ok(())
}

/// 업체 목록을 받지 못했다. 우리 쪽에서 막혔으면 미루고, 다시 물어볼 만하면 재예약, 아니면 이번 대사를 포기한다.
async fn failed_call(
    ctx: &WorkerContext,
    job: &Job,
    provider: &ProviderId,
    error: &ProviderError,
) -> Result<(), WorkerError> {
    let lease = job.lease();
    if let Some(Decision::Defer { wait, reason }) = defer_if_blocked(error, Instant::now()) {
        return defer(ctx, lease, wait, &reason).await;
    }
    tracing::warn!(outcome = error.outcome(), %error, "업체 예약 목록을 받지 못했다");
    let trace = trace::current();
    let exhausted = job.attempts >= job.max_attempts;
    if matches!(error, ProviderError::Client { .. }) || exhausted {
        // 이번 대사는 포기하고(dead, 관리자 확인) 다음 정기 대사는 그대로 돈다
        let reason = format!("reconcile_failed: {error}");
        return end_run(ctx, lease, provider, Some(&reason), &trace).await;
    }
    record_decision("retry");
    let delay = ctx.config.backoff.delay(job.attempts, rand::rng().random());
    let mut conn = ctx.pool.acquire().await?;
    if !jobs::retry_later(&mut conn, lease, delay, &error.to_string()).await? {
        warn_lease_lost();
    }
    Ok(())
}

/// 이번 대사를 끝내고(`dead_reason`이 있으면 dead로) 다음 대사를 한 간격 뒤로 등록한다. 한 트랜잭션이다.
async fn end_run(
    ctx: &WorkerContext,
    lease: Lease,
    provider: &ProviderId,
    dead_reason: Option<&str>,
    trace: &TraceRef,
) -> Result<(), WorkerError> {
    record_decision(if dead_reason.is_some() {
        "give_up"
    } else {
        "reconcile"
    });
    let mut tx = ctx.pool.begin().await?;
    let ended = match dead_reason {
        Some(reason) => jobs::mark_dead(&mut tx, lease, reason).await?,
        None => jobs::complete(&mut tx, lease).await?,
    };
    if !ended {
        warn_lease_lost();
        return Ok(());
    }
    // 이 작업이 끝난 뒤라 업체당 하나뿐인 대사 작업 자리가 비었다
    jobs::schedule_reconcile(
        &mut tx,
        provider,
        ctx.config.reconcile_interval,
        trace.context.as_ref(),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn defer(
    ctx: &WorkerContext,
    lease: Lease,
    wait: Duration,
    reason: &str,
) -> Result<(), WorkerError> {
    record_decision("defer");
    tracing::info!(?wait, reason, "대사를 미룬다 (시도 횟수로 세지 않음)");
    let mut conn = ctx.pool.acquire().await?;
    if !jobs::defer(&mut conn, lease, wait, reason).await? {
        warn_lease_lost();
    }
    Ok(())
}

fn listed_status(status: ListedStatus) -> &'static str {
    match status {
        ListedStatus::Confirmed => "confirmed",
        ListedStatus::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use domain::{BookingRequest, ClientId, Customer};

    use super::*;

    fn booking(status: BookingStatus, provider_booking_id: Option<&str>) -> Booking {
        let now = Utc::now();
        Booking {
            id: BookingId::from_uuid(Uuid::now_v7()),
            client_id: ClientId::from_uuid(Uuid::now_v7()),
            request: BookingRequest {
                provider: ProviderId::parse("alpha").unwrap(),
                resource_id: "store-001".into(),
                slot_start: now + TimeDelta::days(1),
                party_size: 2,
                customer: Customer {
                    name: "홍길동".into(),
                    phone: "010-0000-0000".into(),
                },
            },
            status,
            status_reason: None,
            provider_booking_id: provider_booking_id.map(str::to_owned),
            deadline_at: now,
            created_at: now,
            updated_at: now,
        }
    }

    fn listed(booking: &Booking, id: &str, status: ListedStatus) -> ListedBooking {
        listed_ref(
            &booking.id.to_string(),
            booking.request.slot_start,
            id,
            status,
        )
    }

    fn listed_ref(
        reference: &str,
        slot_start: DateTime<Utc>,
        id: &str,
        status: ListedStatus,
    ) -> ListedBooking {
        ListedBooking {
            provider_booking_id: id.into(),
            reference: Some(reference.into()),
            slot_start,
            status,
        }
    }

    use ListedStatus::{Cancelled, Confirmed};

    #[test]
    fn 양쪽이_같으면_아무것도_찾지_않는다() {
        let ours = booking(BookingStatus::Confirmed, Some("rsv_1"));
        let theirs = vec![listed(&ours, "rsv_1", Confirmed)];
        assert!(compare(&[ours], &theirs).is_empty());
    }

    #[test]
    fn 업체에서_취소된_확정_예약을_찾는다() {
        let ours = booking(BookingStatus::Confirmed, Some("rsv_1"));
        let theirs = vec![listed(&ours, "rsv_1", Cancelled)];
        assert_eq!(
            compare(std::slice::from_ref(&ours), &theirs),
            vec![Finding::CancelledAtProvider {
                booking_id: ours.id,
                provider_booking_id: "rsv_1".into()
            }]
        );
    }

    #[test]
    fn 업체에_없는_확정_예약을_찾는다() {
        let ours = booking(BookingStatus::Confirmed, Some("rsv_1"));
        assert_eq!(
            compare(std::slice::from_ref(&ours), &[]),
            vec![Finding::MissingAtProvider {
                booking_id: ours.id,
                provider_booking_id: Some("rsv_1".into())
            }]
        );
    }

    #[test]
    fn 실패하거나_결과_불명인_예약이_업체에서_확정되어_있으면_찾는다() {
        for status in [BookingStatus::Failed, BookingStatus::Unknown] {
            let ours = booking(status, None);
            let theirs = vec![
                listed(&ours, "rsv_old", Cancelled),
                listed(&ours, "rsv_1", Confirmed),
            ];
            assert_eq!(
                compare(std::slice::from_ref(&ours), &theirs),
                vec![Finding::ConfirmedAtProvider {
                    booking_id: ours.id,
                    from: status,
                    provider_booking_id: "rsv_1".into()
                }],
                "{status}"
            );
        }
    }

    #[test]
    fn 같은_참조값으로_확정된_예약이_둘이면_이중_예약_의심이다() {
        // 처리 중인 예약이어도 이중 예약은 찾는다
        let ours = booking(BookingStatus::Pending, None);
        let theirs = vec![
            listed(&ours, "rsv_1", Confirmed),
            listed(&ours, "rsv_2", Confirmed),
        ];
        assert_eq!(
            compare(std::slice::from_ref(&ours), &theirs),
            vec![Finding::Duplicate {
                booking_id: ours.id,
                provider_booking_ids: vec!["rsv_1".into(), "rsv_2".into()]
            }]
        );
    }

    #[test]
    fn 끝난_예약이_업체에서_확정되어_있으면_상태_불일치다() {
        for status in [BookingStatus::Cancelled, BookingStatus::Rejected] {
            let ours = booking(status, Some("rsv_1"));
            let theirs = vec![listed(&ours, "rsv_1", Confirmed)];
            assert!(
                matches!(
                    compare(std::slice::from_ref(&ours), &theirs).as_slice(),
                    [Finding::Mismatch { ours: s, .. }] if *s == status
                ),
                "{status}"
            );
        }
    }

    #[test]
    fn 업체_예약_id가_다르면_상태_불일치다() {
        let ours = booking(BookingStatus::Confirmed, Some("rsv_1"));
        let theirs = vec![listed(&ours, "rsv_9", Confirmed)];
        assert!(matches!(
            compare(&[ours], &theirs).as_slice(),
            [Finding::Mismatch { provider_booking_id, .. }] if provider_booking_id == "rsv_9"
        ));
    }

    #[test]
    fn 처리_중인_예약은_건너뛴다() {
        for status in [BookingStatus::Pending, BookingStatus::CancelRequested] {
            let ours = booking(status, Some("rsv_1"));
            let theirs = vec![listed(&ours, "rsv_1", Cancelled)];
            assert!(compare(&[ours], &theirs).is_empty(), "{status}");
        }
    }

    #[test]
    fn 우리에게_없는_참조값의_확정_예약을_찾고_다른_경로의_예약은_무시한다() {
        let slot = Utc::now();
        let stranger = BookingId::from_uuid(Uuid::now_v7());
        let theirs = vec![
            listed_ref(&stranger.to_string(), slot, "rsv_1", Confirmed),
            listed_ref(&stranger.to_string(), slot, "rsv_2", Cancelled),
            // 우리 예약 ID 형식이 아닌 참조값 (전화 예약 등)
            listed_ref("PHONE-0001", slot, "rsv_3", Confirmed),
            ListedBooking {
                provider_booking_id: "rsv_4".into(),
                reference: None,
                slot_start: slot,
                status: Confirmed,
            },
        ];
        assert_eq!(
            compare(&[], &theirs),
            vec![Finding::UnknownReference {
                reference: stranger,
                provider_booking_id: "rsv_1".into()
            }]
        );
    }

    #[test]
    fn 취소_실패로_남은_예약이_업체에서_취소되어_있으면_알린다() {
        let ours = booking(BookingStatus::CancelFailed, Some("rsv_1"));
        let theirs = vec![listed(&ours, "rsv_1", Cancelled)];
        assert!(matches!(
            compare(&[ours], &theirs).as_slice(),
            [Finding::Mismatch {
                theirs: Cancelled,
                ..
            }]
        ));
    }
}
