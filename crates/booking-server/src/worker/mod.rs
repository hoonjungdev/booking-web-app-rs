//! 작업 큐 워커 (docs/02-domain-and-data.md §5)
//!
//! **업체마다 워커 루프를 따로 둔다** (Phase 6, docs/decisions.md D-30). 업체 하나에 루프 `concurrency`개(업체 bulkhead
//! 자리 수를 넘지 않게)를 돌리고, 그 루프는 그 업체의 작업만 가져간다. 그래서 alpha가 느려져 alpha 루프가 모두
//! 업체 응답을 기다려도, beta 루프는 그대로 beta 작업을 처리한다 (≈ 업체별 전용 스레드 풀). 이 프로세스에 파이프라인이
//! 없는 업체의 작업(사용하지 않는 업체 등)과 업체가 없는 작업은 따로 둔 루프 하나가 맡아 포기 · 정리한다.
//!
//! 루프 하나는 "작업 하나 가져오기 → 처리 → 다음 작업"을 반복하고, 가져올 작업이 없으면 `poll_interval`만큼 쉰다.
//! 작업을 가져올 때 `FOR UPDATE SKIP LOCKED`를 쓰므로 루프끼리(서버가 여러 대여도) 같은 작업을 동시에 처리하지 않는다.
//!
//! 종료 신호(`CancellationToken`)를 받으면 새 작업을 가져오지 않고, 처리 중이던 작업은 끝까지 마친 뒤 멈춘다.
//!
//! | 작업 | 하는 일 |
//! |---|---|
//! | `submit_booking` | 접수한 예약을 업체에 제출한다 (`submit`) |
//! | `resolve_unknown` | 결과 불명인 예약을 업체에서 조회해 확인한다 (`resolve`) |
//! | `cancel_booking` | 취소 요청을 받은 예약을 업체에서 취소한다 (`cancel`) |
//! | `reconcile_provider` | 업체 예약 목록과 우리 예약을 비교한다. 업체마다 하나씩 정기적으로 돈다 (`reconcile`) |
//!
//! 작업 하나를 처리하는 동안 `job.<종류>` span을 연다. 이 span은 새 trace의 시작이고, 작업을 등록한 요청의 trace와는
//! link로 이어진다 (`crate::trace`, docs/05-observability-and-analytics.md §3).

mod backoff;
mod cancel;
mod reconcile;
mod resolve;
mod submit;

use std::{sync::Arc, time::Duration};

use db::{
    PgPool,
    bookings::{self, Transitioned},
    jobs::{self, ClaimScope, Job, JobKind, Lease},
};
use domain::{Booking, BookingEvent, BookingStatus};
use providers::ProviderError;
use rand::RngExt;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{Span, field::Empty};

pub use backoff::JobBackoff;
pub use cancel::decide_cancel;
pub use reconcile::{Finding, LOOKAHEAD, LOOKBACK, compare};
pub use resolve::{REQUIRED_NOT_FOUND_CHECKS, decide_resolution};
pub use submit::{SubmitPayload, UNRECORDED_SEND, decide, decide_lookup};

use crate::{metrics, registry::ProviderRegistry, trace};

/// 워커 이름. `booking_events.actor`에 남는다.
const ACTOR: &str = "worker";
/// 워커 lease에 업체 호출 시간 말고도 더 남겨 둘 여유 (작업 가져오기 · 결과 저장 같은 DB 작업).
/// lease는 `업체 호출 한 번의 최대 시간 + 이 값` 이상이어야 한다 (서버 시작 검사, 정책 변경 검사).
pub const LEASE_MARGIN: Duration = Duration::from_secs(5);
/// 미룬 작업을 다시 꺼내기까지 최소한 기다리는 시간 (같은 작업을 너무 자주 꺼내지 않도록)
const MIN_DEFER: Duration = Duration::from_millis(200);
/// 미룬 작업들이 같은 순간에 몰려 돌아오지 않도록 더하는 무작위 시간의 최대값
const DEFER_JITTER: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy)]
pub struct WorkerConfig {
    /// 업체 하나당 동시에 처리할 작업 수 (업체마다 이만큼의 루프. 업체 bulkhead 자리 수를 넘으면 그 수로 줄인다)
    pub concurrency: usize,
    /// 할 일이 없을 때 다시 확인하기까지 쉬는 시간
    pub poll_interval: Duration,
    /// 작업 하나를 쥐고 있을 수 있는 시간. 넘기면 다른 워커가 회수한다.
    pub lease: Duration,
    /// 실패한 작업을 다시 시도하기까지의 간격
    pub backoff: JobBackoff,
    /// 결과 불명 예약을 업체에서 찾지 못했을 때, 다시 조회하기까지 기다리는 시간 (docs/03-resilience.md §5)
    pub unknown_recheck_interval: Duration,
    /// 업체마다 대사를 도는 간격 (docs/03-resilience.md §9)
    pub reconcile_interval: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            concurrency: 4,
            poll_interval: Duration::from_millis(500),
            lease: Duration::from_secs(60),
            backoff: JobBackoff::default(),
            unknown_recheck_interval: Duration::from_secs(30),
            reconcile_interval: Duration::from_secs(600),
        }
    }
}

/// 워커가 공유하는 의존성 (≈ C#에서 생성자로 주입받는 서비스 묶음)
#[derive(Clone)]
pub struct WorkerContext {
    pub pool: PgPool,
    pub providers: Arc<ProviderRegistry>,
    pub config: WorkerConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Db(#[from] db::DbError),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// 업체 호출 결과를 보고 정한 다음 행동. 판단(`decide` 함수들)과 실행([`apply`])을 나눠
/// 판단은 DB 없이 단위 테스트한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 이 사건으로 예약 상태를 바꾸고 작업을 끝낸다. 바뀐 상태에 이어서 할 작업이 있으면 함께 등록한다.
    Transition(BookingEvent),
    /// 나중에 다시 시도한다. 시도 횟수를 하나 쓴다 (사유)
    Retry(String),
    /// 업체를 부르지 않고 우리 쪽에서 막혔다 (서킷 열림, 대기 한도 초과, 업체 pause).
    /// 시도 횟수를 쓰지 않고 `wait` 뒤로 미룬다.
    Defer { wait: Duration, reason: String },
    /// 결과 불명 예약이 업체에 아직 없다. 확인한 횟수를 남기고 잠시 뒤 다시 조회한다.
    Recheck { not_found_checks: u32 },
    /// 더 시도하지 않고 포기한다 (사유)
    GiveUp(String),
}

/// 우리 쪽에서 막힌 실패면 `Defer`를 만든다 (submit · resolve가 함께 쓰는 규칙)
fn defer_if_blocked(error: &ProviderError, now: std::time::Instant) -> Option<Decision> {
    error.blocked_until().map(|retry_at| Decision::Defer {
        wait: retry_at.saturating_duration_since(now),
        reason: error.to_string(),
    })
}

/// 종료 신호가 올 때까지 워커 루프들을 돌린다.
///
/// 업체마다 루프를 `concurrency`개(업체 bulkhead 자리 수를 넘지 않게) 띄우고, 업체 파이프라인이 없는 작업을 맡을 루프를
/// 하나 더 띄운다. 시작할 때 대사를 지원하는 업체마다 정기 대사 작업이 있는지 확인하고 없으면 등록한다.
pub async fn run(ctx: WorkerContext, shutdown: CancellationToken) {
    schedule_reconciliations(&ctx).await;

    // JoinSet: 여러 태스크를 띄우고 모두 끝나기를 기다리는 묶음 (≈ C#의 `Task.WhenAll`)
    let mut loops = JoinSet::new();
    let assigned: Vec<_> = ctx.providers.ids().into_iter().cloned().collect();
    for provider in ctx.providers.all() {
        let bulkhead = provider.policy().bulkhead.max_concurrency.max(1) as usize;
        let count = ctx.config.concurrency.clamp(1, bulkhead);
        for index in 0..count {
            let scope = ClaimScope::Provider(provider.id().clone());
            loops.spawn(run_loop(ctx.clone(), shutdown.clone(), scope, index));
        }
        tracing::info!(provider = %provider.id(), loops = count, "업체 워커 루프를 띄운다");
    }
    let unassigned = ClaimScope::Unassigned(assigned);
    loops.spawn(run_loop(ctx.clone(), shutdown.clone(), unassigned, 0));
    tracing::info!(concurrency = ctx.config.concurrency, "worker started");

    while let Some(result) = loops.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "worker loop panicked");
        }
    }
    tracing::info!("worker stopped");
}

/// 대사를 지원하는 업체마다 정기 대사 작업이 대기 · 실행 중인지 확인하고, 없으면 한 간격 뒤로 등록한다.
///
/// 대사 작업은 끝날 때 다음 실행을 스스로 등록한다 (작업 체인). 여기서는 처음 시작할 때나, 작업이 묻혀 사슬이 끊겼을 때 잇는다.
async fn schedule_reconciliations(ctx: &WorkerContext) {
    let mut conn = match ctx.pool.acquire().await {
        Ok(conn) => conn,
        Err(error) => {
            tracing::error!(%error, "정기 대사 작업을 등록하지 못했다");
            return;
        }
    };
    for provider in ctx.providers.all() {
        if !provider.capabilities().list_bookings {
            continue;
        }
        let interval = ctx.config.reconcile_interval;
        match jobs::schedule_reconcile(&mut conn, provider.id(), interval, None).await {
            Ok(Some(job_id)) => {
                tracing::info!(provider = %provider.id(), job_id, "정기 대사 작업을 등록했다");
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(provider = %provider.id(), %error, "정기 대사 작업을 등록하지 못했다");
            }
        }
    }
}

async fn run_loop(
    ctx: WorkerContext,
    shutdown: CancellationToken,
    scope: ClaimScope,
    index: usize,
) {
    while !shutdown.is_cancelled() {
        let idle = match process_next_in(&ctx, &scope).await {
            Ok(processed) => !processed,
            Err(error) => {
                // DB가 잠깐 끊기는 등의 오류. 루프는 멈추지 않고 잠시 쉰 뒤 다시 시도한다.
                tracing::error!(loop_index = index, ?scope, %error, "작업 처리 중 오류");
                true
            }
        };
        if idle {
            // 둘 중 먼저 끝나는 쪽: 종료 신호가 오면 바로 빠져나가고, 아니면 poll_interval만큼 쉰다
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(ctx.config.poll_interval) => {}
            }
        }
    }
}

/// 실행할 작업 하나를 (업체를 가리지 않고) 가져와 처리한다. 처리할 작업이 없었으면 `Ok(false)`.
///
/// 테스트는 루프를 돌리지 않고 이 함수를 직접 불러 한 단계씩 확인한다.
pub async fn process_next(ctx: &WorkerContext) -> Result<bool, WorkerError> {
    process_next_in(ctx, &ClaimScope::Any).await
}

/// `scope`에 맞는 작업 하나를 가져와 처리한다. 처리할 작업이 없었으면 `Ok(false)`.
pub async fn process_next_in(ctx: &WorkerContext, scope: &ClaimScope) -> Result<bool, WorkerError> {
    let Some(job) = jobs::claim_next(&ctx.pool, ctx.config.lease, scope).await? else {
        return Ok(false);
    };
    metrics::job_claimed(&job);
    process(ctx, job).await?;
    Ok(true)
}

/// `#[tracing::instrument]`: 이 함수 실행 동안 `job` span을 열어, 안에서 남기는 로그에 job_id 등이 함께 찍히게 한다.
///
/// - `otel.name`: Grafana에 보이는 span 이름. 작업 종류를 넣어 `job.submit_booking`처럼 만든다.
/// - `Empty`로 둔 필드는 처리하면서 채운다: `booking_id`, `trace_id`, 이번 실행의 결론(`decision`),
///   포기했을 때의 오류 표시(`otel.status_code`).
#[tracing::instrument(
    name = "job",
    skip_all,
    fields(
        otel.name = %format!("job.{}", job.kind),
        otel.status_code = Empty,
        job_id = job.id,
        kind = %job.kind,
        attempt = job.attempts,
        booking_id = Empty,
        decision = Empty,
        trace_id = Empty,
    )
)]
async fn process(ctx: &WorkerContext, job: Job) -> Result<(), WorkerError> {
    // 이 작업을 등록한 요청(또는 앞선 작업)의 trace로 가는 link를 걸고, 이 trace의 ID를 로그에 보이게 남긴다
    let span = Span::current();
    trace::link_to_origin(&span, &job);
    if let Some(trace_id) = telemetry::current_trace_id() {
        span.record("trace_id", trace_id.as_str());
    }

    match job.kind.parse::<JobKind>() {
        Ok(JobKind::SubmitBooking) => submit::handle(ctx, &job).await,
        Ok(JobKind::ResolveUnknown) => resolve::handle(ctx, &job).await,
        Ok(JobKind::CancelBooking) => cancel::handle(ctx, &job).await,
        Ok(JobKind::ReconcileProvider) => reconcile::handle(ctx, &job).await,
        Err(_) => {
            tracing::error!("알 수 없는 작업 종류라 처리하지 않는다");
            record_decision("bury");
            let mut conn = ctx.pool.acquire().await?;
            jobs::mark_dead(&mut conn, job.lease(), "unsupported job kind").await?;
            Ok(())
        }
    }
}

/// 작업의 예약을 읽는다. 작업에 예약이 없거나 예약을 찾을 수 없으면 작업을 `dead`로 묻고 `None`.
async fn load_booking(ctx: &WorkerContext, job: &Job) -> Result<Option<Booking>, WorkerError> {
    let Some(booking_id) = job.booking_id else {
        bury(ctx, job.lease(), "작업에 booking_id가 없습니다").await?;
        return Ok(None);
    };
    Span::current().record("booking_id", tracing::field::display(booking_id));

    let booking = bookings::get(&ctx.pool, booking_id).await?;
    if booking.is_none() {
        bury(ctx, job.lease(), "작업의 예약을 찾을 수 없습니다").await?;
    }
    Ok(booking)
}

/// 판단한 행동을 DB에 반영한다
async fn apply(
    ctx: &WorkerContext,
    job: &Job,
    booking: &Booking,
    decision: Decision,
) -> Result<(), WorkerError> {
    let lease = job.lease();
    let exhausted = job.attempts >= job.max_attempts;
    match decision {
        Decision::Transition(event) => finish(ctx, lease, booking, &event).await,
        Decision::GiveUp(reason) => give_up(ctx, lease, booking, reason).await,
        Decision::Retry(reason) if exhausted => {
            give_up(ctx, lease, booking, format!("attempts_exhausted: {reason}")).await
        }
        Decision::Retry(reason) => {
            record_decision("retry");
            let delay = ctx.config.backoff.delay(job.attempts, rand::rng().random());
            tracing::info!(?delay, %reason, "나중에 다시 시도한다");
            let mut conn = ctx.pool.acquire().await?;
            if !jobs::retry_later(&mut conn, lease, delay, &reason).await? {
                warn_lease_lost();
            }
            Ok(())
        }
        Decision::Defer { wait, reason } => {
            record_decision("defer");
            let delay = wait.max(MIN_DEFER) + DEFER_JITTER.mul_f64(rand::rng().random());
            tracing::info!(?delay, %reason, "업체를 부르지 않고 미룬다 (시도 횟수로 세지 않음)");
            let mut conn = ctx.pool.acquire().await?;
            if !jobs::defer(&mut conn, lease, delay, &reason).await? {
                warn_lease_lost();
            }
            Ok(())
        }
        Decision::Recheck { .. } if exhausted => {
            let reason = "attempts_exhausted: not_found_at_provider".to_owned();
            give_up(ctx, lease, booking, reason).await
        }
        Decision::Recheck { not_found_checks } => {
            record_decision("recheck");
            let delay = ctx.config.unknown_recheck_interval;
            tracing::info!(
                ?delay,
                not_found_checks,
                "업체에 아직 없다. 잠시 뒤 다시 조회한다"
            );
            let payload = serde_json::json!({ "not_found_checks": not_found_checks });
            let mut conn = ctx.pool.acquire().await?;
            if !jobs::retry_later_with_payload(
                &mut conn,
                lease,
                delay,
                "not_found_at_provider",
                &payload,
            )
            .await?
            {
                warn_lease_lost();
            }
            Ok(())
        }
    }
}

/// 예약 상태 전이, 작업 완료, 이어서 할 작업 등록을 한 트랜잭션으로 저장한다.
/// 하나만 저장되면 "확정됐는데 작업이 또 도는" 식의 불일치나, 상태만 바뀌고 처리할 작업이 없는 유실이 생기기 때문이다.
async fn finish(
    ctx: &WorkerContext,
    lease: Lease,
    booking: &Booking,
    event: &BookingEvent,
) -> Result<(), WorkerError> {
    record_decision("transition");
    // 이 작업의 trace. 상태 이력에 남기고, 이어서 할 작업에도 실어 보낸다 (그 작업이 이 trace로 link를 건다).
    let trace = trace::current();

    let mut tx = ctx.pool.begin().await?;
    if !jobs::complete(&mut tx, lease).await? {
        warn_lease_lost();
        return Ok(()); // tx가 commit 없이 버려지면서 롤백된다
    }
    let transitioned = bookings::transition(
        &mut tx,
        booking.id,
        booking.status,
        event,
        ACTOR,
        trace.trace_id.as_deref(),
    )
    .await?;
    if let Transitioned::Applied(to) = transitioned
        && let Some(kind) = follow_up(to)
    {
        jobs::enqueue(
            &mut tx,
            kind,
            Some(booking.id),
            jobs::DEFAULT_MAX_ATTEMPTS,
            trace.context.as_ref(),
        )
        .await?;
    }
    tx.commit().await?;

    // 지표는 커밋한 뒤에 센다. 커밋이 실패하면 전이는 일어나지 않은 것이다.
    match transitioned {
        Transitioned::Applied(to) => {
            metrics::booking_transitioned(booking, to);
            tracing::info!(status = %to, "예약 상태를 바꿨다");
        }
        Transitioned::Conflict => tracing::warn!("그사이 예약 상태가 바뀌어 전이를 건너뛴다"),
    }
    Ok(())
}

/// 예약이 이 상태가 되면 이어서 처리할 작업.
/// `pending`이 되면 업체에 (다시) 제출하고, `unknown`이 되면 업체에서 조회해 확인하고, `cancel_requested`가 되면 업체에서 취소한다.
pub fn follow_up(status: BookingStatus) -> Option<JobKind> {
    match status {
        BookingStatus::Pending => Some(JobKind::SubmitBooking),
        BookingStatus::Unknown => Some(JobKind::ResolveUnknown),
        BookingStatus::CancelRequested => Some(JobKind::CancelBooking),
        BookingStatus::Confirmed
        | BookingStatus::Rejected
        | BookingStatus::Failed
        | BookingStatus::Cancelled
        | BookingStatus::CancelFailed => None,
    }
}

/// 포기할 때의 사건: 취소 흐름(`cancel_requested`)이면 취소 실패, 아니면 제출 실패
fn give_up_event(status: BookingStatus, reason: String) -> BookingEvent {
    match status {
        BookingStatus::CancelRequested => BookingEvent::CancelGaveUp { reason },
        _ => BookingEvent::GaveUp { reason },
    }
}

/// 예약을 `failed`(취소 흐름이면 `cancel_failed`)로, 작업을 `dead`로 만든다 (한 트랜잭션)
async fn give_up(
    ctx: &WorkerContext,
    lease: Lease,
    booking: &Booking,
    reason: String,
) -> Result<(), WorkerError> {
    let trace_id = telemetry::current_trace_id();

    let mut tx = ctx.pool.begin().await?;
    if !jobs::mark_dead(&mut tx, lease, &reason).await? {
        warn_lease_lost();
        return Ok(());
    }
    let event = give_up_event(booking.status, reason.clone());
    let transitioned = bookings::transition(
        &mut tx,
        booking.id,
        booking.status,
        &event,
        ACTOR,
        trace_id.as_deref(),
    )
    .await?;
    let Transitioned::Applied(to) = transitioned else {
        // 그사이 다른 경로(대사가 업체에서 확정된 예약을 찾음 등)가 예약을 처리했다. 포기할 것이 없으므로 작업을 dead로
        // 남기지 않고 끝내기만 한다 (dead로 남기면 예약은 멀쩡한데 "dead 작업이 있음" 알림이 계속 울린다).
        // `tx`는 커밋하지 않고 버리므로 dead 표시는 롤백된다.
        drop(tx);
        tracing::info!(status = %booking.status, "그사이 예약 상태가 바뀌어 포기하지 않고 작업만 끝낸다");
        return complete_only(ctx, lease).await;
    };
    tx.commit().await?;

    tracing::warn!(%reason, "예약 처리를 포기했다");
    record_decision("give_up");
    // 포기한 작업은 trace에서 오류로 표시한다 (Grafana에서 오류 span만 골라 볼 수 있다)
    Span::current().record("otel.status_code", "error");
    metrics::booking_transitioned(booking, to);
    Ok(())
}

/// 이미 다른 경로로 처리된 예약의 작업: 예약은 두고 작업만 끝낸다
async fn complete_only(ctx: &WorkerContext, lease: Lease) -> Result<(), WorkerError> {
    record_decision("skip");
    let mut conn = ctx.pool.acquire().await?;
    if !jobs::complete(&mut conn, lease).await? {
        warn_lease_lost();
    }
    Ok(())
}

/// 예약이 없는 등 작업 자체가 잘못된 경우: 작업만 `dead`로 남긴다
async fn bury(ctx: &WorkerContext, lease: Lease, reason: &str) -> Result<(), WorkerError> {
    tracing::error!(%reason, "처리할 수 없는 작업");
    record_decision("bury");
    let mut conn = ctx.pool.acquire().await?;
    jobs::mark_dead(&mut conn, lease, reason).await?;
    Ok(())
}

/// 작업 span에 이번 실행의 결론을 남긴다. trace를 열면 "이 작업은 어떻게 끝났나"가 바로 보인다.
///
/// 이 모듈의 함수들은 작업 span 안에서 바로 실행되므로(따로 span을 열지 않는다) 지금 span이 곧 작업 span이다.
fn record_decision(decision: &'static str) {
    Span::current().record("decision", decision);
}

fn warn_lease_lost() {
    tracing::warn!("lease를 잃어 결과를 반영하지 않는다 (다른 워커가 이 작업을 다시 가져감)");
}
