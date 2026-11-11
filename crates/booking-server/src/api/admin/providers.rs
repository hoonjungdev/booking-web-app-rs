//! 관리자: 업체 목록 · 상세(정책 · 런타임 상태) · 정책 변경 · 서킷 강제 조작 · 지금 대사

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use db::providers::ProviderRecord;
use domain::ProviderId;
use providers::{
    BulkheadPolicy, CircuitAction, CircuitBreakerPolicy, CircuitState, ForcedState, ProviderPolicy,
    RateLimitPolicy, ResilientProvider, RetryPolicy, RuntimeStatus,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{JobView, find_provider, record_audit, to_utc};

/// 정책 변경을 이 프로세스 안에서 한 번에 하나씩 하게 하는 잠금 (`update_policy` 참고).
/// `tokio::sync::Mutex`: 잠근 채로 `await`(DB 저장)할 수 있는 async용 잠금이다 (std `Mutex`는 그러면 안 된다).
static POLICY_UPDATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
use crate::{
    admin_auth::{AdminSession, RequireAdmin, RequireOperator},
    api::{ApiError, ApiState, Problem, docs::tags},
    trace,
    worker::LEASE_MARGIN,
};

/// 업체 한 곳 (목록의 한 줄)
#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderView {
    pub id: ProviderId,
    pub display_name: String,
    /// 어떤 어댑터 코드로 호출하는가 (`alpha_v1`, `beta_v1` …)
    pub adapter_kind: String,
    pub base_url: String,
    /// 예약을 받는가. 끄면 서버를 다시 시작할 때 업체 파이프라인을 만들지 않는다
    pub enabled: bool,
    /// 설정을 마지막으로 바꾼 시각
    pub updated_at: DateTime<Utc>,
    /// 업체 파이프라인의 지금 상태. 이 프로세스에 파이프라인이 없으면(`--role api`, 사용하지 않는 업체) `null`
    pub runtime: Option<RuntimeView>,
}

/// 업체 상세: 목록 정보 + 정책
#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderDetail {
    pub provider: ProviderView,
    /// 지금 적용 중인 장애 대응 정책 (빠진 값은 기본값으로 채운 것). 업체 파이프라인이 있으면 그 정책,
    /// 없으면 DB의 정책이다
    pub policy: PolicyView,
    /// DB의 정책이 지금 적용 중인 정책과 다르다. 관리자 API로 바꾸면 바로 반영되므로, DB를 직접 고쳤거나
    /// 다른 프로세스에서 바꾼 경우다. 서버를 다시 시작하면 반영된다
    pub restart_required: bool,
}

/// 업체 파이프라인의 지금 상태 (이 프로세스 메모리의 값)
#[derive(Debug, Serialize, ToSchema)]
pub struct RuntimeView {
    pub circuit: CircuitView,
    /// 업체가 쉬라고 해서(`Retry-After`) 호출을 멈춘 상태면 그 끝 시각
    pub paused_until: Option<DateTime<Utc>>,
    /// 지금 업체를 부르고 있는 호출 수 / bulkhead 전체 자리 수
    pub in_flight: u32,
    pub max_concurrency: u32,
    /// 우리 rate limiter 버킷에 남은 토큰 수 (추정값) / 버킷 크기 (적응형 조절로 속도와 함께 줄어든다)
    pub limiter_available: u32,
    pub limiter_burst: u32,
    /// 우리 rate limiter가 지금 허용하는 초당 호출 수. 업체가 한도 초과를 알리면 절반으로 줄었다가 조금씩 되돌아간다 (AIMD)
    pub rate_limit_allowed: u32,
    /// 정책이 정한 초당 호출 수 상한
    pub rate_limit_ceiling: u32,
    /// 업체가 마지막 응답에서 알려 준 남은 호출 수 (`X-RateLimit-Remaining`). 받은 적이 없으면 `null`
    pub rate_limit_remaining: Option<u32>,
}

/// 서킷 브레이커 상태 (docs/03-resilience.md §7)
#[derive(Debug, Serialize, ToSchema)]
pub struct CircuitView {
    pub state: CircuitStateView,
    /// 관리자가 강제로 정한 상태면 그 값. 강제된 동안에는 호출 결과로 상태가 바뀌지 않는다
    pub forced: Option<ForcedStateView>,
    /// 닫힌 상태: 실패율 계산에 쓰는 최근 결과 수와 그중 실패 수
    pub recent_calls: u32,
    pub recent_failures: u32,
    /// 열린 상태: 시험 호출(half-open)을 시작할 수 있게 되는 시각. 강제로 열었으면 `null`
    pub open_until: Option<DateTime<Utc>>,
    /// 반만 열린 상태: 성공한 시험 호출 수와 결과를 기다리는 시험 호출 수
    pub probes_succeeded: u32,
    pub probes_in_flight: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CircuitStateView {
    /// 정상: 모든 호출을 보낸다
    Closed,
    /// 차단: 호출하지 않고 바로 실패시킨다
    Open,
    /// 시험 중: 정해진 수의 시험 호출만 보낸다
    HalfOpen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ForcedStateView {
    Open,
    Closed,
}

/// 서킷 조작 요청
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CircuitCommand {
    pub action: CircuitActionView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CircuitActionView {
    /// 강제로 연다. 풀 때까지 호출하지 않는다 (업체 점검 공지에 대응)
    ForceOpen,
    /// 강제로 닫는다. 풀 때까지 실패가 쌓여도 열리지 않는다
    ForceClose,
    /// 강제 상태를 풀고 닫힌 상태에서 처음부터 다시 센다
    Reset,
}

/// 장애 대응 정책 (docs/03-resilience.md §3). 응답이자 정책 변경(PUT) 요청 본문이다 (모든 필드가 필요하다).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyView {
    /// 시도 한 번의 최대 시간 (최대 60000)
    pub attempt_timeout_ms: u64,
    /// 호출 한 번(재시도 포함)에 쓸 수 있는 전체 시간
    pub call_budget_ms: u64,
    pub retry: RetryPolicyView,
    pub rate_limit: RateLimitPolicyView,
    pub bulkhead: BulkheadPolicyView,
    pub circuit_breaker: CircuitBreakerPolicyView,
    /// 빈 시간 조회 결과를 캐시에서 새것으로 보는 시간 (업체 호출이 실패하면 5분까지는 캐시 값을 준다)
    pub availability_cache_ttl_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicyView {
    /// 첫 시도를 포함한 최대 시도 횟수
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitPolicyView {
    /// 초당 허용 호출 수 (적응형이면 상한)
    pub per_second: u32,
    /// 한꺼번에 보낼 수 있는 호출 수 (버킷 크기)
    pub burst: u32,
    /// 토큰을 기다리는 최대 시간
    pub max_wait_ms: u64,
    /// 적응형 조절 (AIMD): 업체가 한도 초과를 알리면 속도를 절반으로, 한동안 문제없으면 조금씩 되돌린다
    pub adaptive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BulkheadPolicyView {
    pub max_concurrency: u32,
    pub max_wait_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CircuitBreakerPolicyView {
    /// 최근 몇 번의 결과로 실패율을 계산하는가
    pub window: u32,
    /// 이만큼은 호출해 봐야 판단한다
    pub min_calls: u32,
    /// 이 비율 이상 실패하면 연다 (0 초과 ~ 1)
    pub failure_rate: f64,
    /// 열린 뒤 시험 호출을 허용하기까지 기다리는 시간
    pub open_ms: u64,
    /// 반만 열린 상태의 시험 호출 수
    pub half_open_probes: u32,
}

/// 업체 목록
#[utoipa::path(
    get,
    path = "/providers",
    operation_id = "admin_list_providers",
    tag = tags::ADMIN_PROVIDERS,
    security(("admin_session" = [])),
    responses(
        (status = 200, description = "모든 업체 (사용하지 않는 업체 포함)", body = Vec<ProviderView>),
        (status = 401, description = "로그인 필요", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.providers.list", skip_all)]
pub async fn list_providers(
    State(state): State<ApiState>,
    _session: AdminSession,
) -> Result<Json<Vec<ProviderView>>, ApiError> {
    let records = db::providers::list_all(&state.pool).await?;
    Ok(Json(
        records
            .into_iter()
            .map(|record| provider_view(&state, record))
            .collect(),
    ))
}

/// 업체 상세
#[utoipa::path(
    get,
    path = "/providers/{id}",
    operation_id = "admin_get_provider",
    tag = tags::ADMIN_PROVIDERS,
    security(("admin_session" = [])),
    params(("id" = String, Path, description = "업체 ID")),
    responses(
        (status = 200, description = "업체 설정 · 정책 · 런타임 상태", body = ProviderDetail),
        (status = 401, description = "로그인 필요", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "없는 업체", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.providers.get", skip_all)]
pub async fn get_provider(
    State(state): State<ApiState>,
    _session: AdminSession,
    Path(id): Path<String>,
) -> Result<Json<ProviderDetail>, ApiError> {
    let record = find_provider(&state, &id).await?;
    Ok(Json(provider_detail(&state, record)?))
}

/// 업체 정책을 바꾼다 (즉시 반영)
///
/// DB에 저장하고, 이 프로세스의 업체 파이프라인에 바로 적용한다 (다음 호출부터. 진행 중인 호출은 시작할 때의 정책으로 끝난다).
/// 바꾼 정책으로는 업체 호출 한 번이 워커의 lease보다 길어지면 거절한다 (호출 도중 다른 워커가 같은 작업을 가져가면
/// 멱등키 없는 업체에서는 이중 예약이 된다). admin 역할만 쓸 수 있다.
#[utoipa::path(
    put,
    path = "/providers/{id}/policy",
    operation_id = "admin_update_policy",
    tag = tags::ADMIN_PROVIDERS,
    security(("admin_session" = [])),
    params(("id" = String, Path, description = "업체 ID")),
    request_body = PolicyView,
    responses(
        (status = 200, description = "바꾼 뒤의 업체 상세", body = ProviderDetail),
        (status = 400, description = "정책 값 오류 (0인 값, 범위 밖 비율, lease보다 긴 호출 등)", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "로그인 필요", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "admin 권한 필요", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "없는 업체", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.providers.policy", skip_all, fields(provider = %id))]
pub async fn update_policy(
    State(state): State<ApiState>,
    RequireAdmin(session): RequireAdmin,
    Path(id): Path<String>,
    body: Result<Json<PolicyView>, JsonRejection>,
) -> Result<Json<ProviderDetail>, ApiError> {
    let Json(view) = body?;
    let record = find_provider(&state, &id).await?;
    let policy = ProviderPolicy::from(view);
    policy.validate().map_err(ApiError::InvalidRequest)?;
    if let Some(lease) = state.worker_lease {
        let needed = policy.longest_call() + LEASE_MARGIN;
        if needed > lease {
            return Err(ApiError::InvalidRequest(format!(
                "업체 호출 한 번이 최대 {:.1}초 걸릴 수 있어 워커 lease({}초)를 넘습니다. \
                 call_budget_ms · rate_limit.max_wait_ms · bulkhead.max_wait_ms를 줄이세요 \
                 (또는 BOOKING_WORKER_LEASE_SECS를 늘려 서버를 다시 시작하세요)",
                needed.as_secs_f64(),
                lease.as_secs()
            )));
        }
    }
    let json =
        serde_json::to_value(&policy).map_err(|error| ApiError::Internal(error.to_string()))?;

    // 정책 변경은 이 프로세스 안에서 한 번에 하나씩 한다 (저장부터 파이프라인 적용까지). 두 변경이 겹치면
    // DB에 커밋된 순서와 파이프라인에 적용되는 순서가 달라져, 화면 · DB의 정책과 실제로 쓰는 정책이 어긋날 수 있다.
    let _serialized = POLICY_UPDATE.lock().await;
    // 바꾸기 전 정책은 잠근 뒤에 읽는다 (감사 로그의 before가 바로 앞 변경의 결과와 맞게)
    let stored = db::providers::find(&state.pool, &record.id)
        .await?
        .and_then(|current| ProviderPolicy::from_json(current.policy).ok());
    let actor = session.actor();
    let mut tx = state.pool.begin().await?;
    db::providers::update_policy(&mut tx, &record.id, &json)
        .await?
        .ok_or_else(|| ApiError::ProviderNotFound(id.clone()))?;
    let detail = serde_json::json!({
        "before": stored.as_ref().map(PolicyView::from),
        "after": PolicyView::from(&policy),
    });
    record_audit(
        &mut tx,
        &actor,
        "provider.policy.update",
        Some(record.id.as_str()),
        detail,
    )
    .await?;
    tx.commit().await?;

    // 커밋한 뒤에 파이프라인에 적용한다 (저장에 실패했는데 적용만 되는 일이 없게)
    if let Some(provider) = runtime(&state, &record.id) {
        provider.update_policy(policy);
    }
    tracing::info!(provider = %record.id, %actor, "관리자가 업체 정책을 바꿨다");
    let record = find_provider(&state, &id).await?;
    Ok(Json(provider_detail(&state, record)?))
}

/// 서킷을 강제로 열거나 닫거나, 강제 상태를 푼다
///
/// 서킷 상태는 업체를 부르는 프로세스의 메모리에 있다. 워커와 같은 프로세스(`--role all`)에서만 조작할 수 있다.
#[utoipa::path(
    post,
    path = "/providers/{id}/circuit",
    operation_id = "admin_control_circuit",
    tag = tags::ADMIN_PROVIDERS,
    security(("admin_session" = [])),
    params(("id" = String, Path, description = "업체 ID")),
    request_body = CircuitCommand,
    responses(
        (status = 200, description = "조작한 뒤의 런타임 상태", body = RuntimeView),
        (status = 400, description = "요청 본문 오류", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "로그인 필요", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "operator 권한 필요", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "없는 업체", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "사용하지 않는 업체 (파이프라인이 없음)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "이 프로세스에 업체 파이프라인이 없음 (--role api)", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.providers.circuit", skip_all, fields(provider = %id))]
pub async fn control_circuit(
    State(state): State<ApiState>,
    RequireOperator(session): RequireOperator,
    Path(id): Path<String>,
    body: Result<Json<CircuitCommand>, JsonRejection>,
) -> Result<Json<RuntimeView>, ApiError> {
    let Json(command) = body?;
    let provider = pipeline(&state, &id).await?;
    // 서킷은 DB 밖(프로세스 메모리)의 상태라 조작과 감사 로그를 한 트랜잭션에 묶을 수 없다. 기록을 먼저 남기고
    // 조작한다: 기록에 실패하면 조작하지 않으므로 "조작은 됐는데 기록이 없는" 일이 없다 (조작 자체는 실패하지 않는다).
    let mut conn = state.pool.acquire().await?;
    record_audit(
        &mut conn,
        &session.actor(),
        "provider.circuit",
        Some(provider.id().as_str()),
        serde_json::json!({ "action": command.action }),
    )
    .await?;
    provider.control_circuit(match command.action {
        CircuitActionView::ForceOpen => CircuitAction::ForceOpen,
        CircuitActionView::ForceClose => CircuitAction::ForceClose,
        CircuitActionView::Reset => CircuitAction::Reset,
    });
    Ok(Json(RuntimeView::from(provider.runtime_status())))
}

/// 업체의 대사를 지금 돌린다 (다음 정기 대사를 기다리지 않고)
///
/// 대기 중인 대사 작업의 실행 시각을 지금으로 당기고, 없으면 새로 등록한다. 이미 실행 중이면 그 작업을 돌려준다.
#[utoipa::path(
    post,
    path = "/providers/{id}/reconcile",
    operation_id = "admin_run_reconcile",
    tag = tags::ADMIN_RECONCILIATION,
    security(("admin_session" = [])),
    params(("id" = String, Path, description = "업체 ID")),
    responses(
        (status = 202, description = "대사 작업 (곧 워커가 처리한다)", body = JobView),
        (status = 401, description = "로그인 필요", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "operator 권한 필요", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "없는 업체", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "기간별 예약 목록을 지원하지 않는 업체", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.providers.reconcile", skip_all, fields(provider = %id))]
pub async fn run_reconcile(
    State(state): State<ApiState>,
    RequireOperator(session): RequireOperator,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<JobView>), ApiError> {
    let record = find_provider(&state, &id).await?;
    if let Some(provider) = runtime(&state, &record.id)
        && !provider.capabilities().list_bookings
    {
        return Err(ApiError::Conflict(format!(
            "업체 {id}는 기간별 예약 목록을 지원하지 않아 대사를 할 수 없습니다"
        )));
    }
    let trace = trace::current();
    let mut tx = state.pool.begin().await?;
    let job = db::jobs::run_reconcile_now(&mut tx, &record.id, trace.context.as_ref()).await?;
    record_audit(
        &mut tx,
        &session.actor(),
        "provider.reconcile",
        Some(record.id.as_str()),
        serde_json::json!({ "job_id": job.id }),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(provider = %record.id, job_id = job.id, "관리자가 대사를 지금 돌린다");
    Ok((StatusCode::ACCEPTED, Json(JobView::try_from(job)?)))
}

/// 업체 상세를 만든다. 화면에는 지금 실제로 적용 중인 정책을 보여 준다.
fn provider_detail(state: &ApiState, record: ProviderRecord) -> Result<ProviderDetail, ApiError> {
    let stored = ProviderPolicy::from_json(record.policy.clone()).map_err(|error| {
        ApiError::Internal(format!(
            "업체 {}의 정책을 읽을 수 없습니다: {error}",
            record.id
        ))
    })?;
    let running = runtime(state, &record.id).map(|provider| provider.policy());
    let restart_required = running.as_ref().is_some_and(|running| **running != stored);
    let policy = running.map_or(stored, |running| (*running).clone());
    Ok(ProviderDetail {
        policy: PolicyView::from(&policy),
        restart_required,
        provider: provider_view(state, record),
    })
}

/// 이 프로세스의 업체 파이프라인 (없으면 `None`)
fn runtime(state: &ApiState, id: &ProviderId) -> Option<Arc<ResilientProvider>> {
    state
        .providers
        .as_ref()
        .and_then(|registry| registry.get(id))
}

/// 이 프로세스의 업체 파이프라인. 없는 이유에 따라 다른 오류를 돌려준다.
async fn pipeline(state: &ApiState, id: &str) -> Result<Arc<ResilientProvider>, ApiError> {
    let record = find_provider(state, id).await?;
    let registry = state
        .providers
        .as_ref()
        .ok_or(ApiError::ProviderRuntimeUnavailable)?;
    registry.get(&record.id).ok_or_else(|| {
        ApiError::Conflict(format!(
            "업체 {id}의 파이프라인이 없습니다 (사용하지 않는 업체이거나, 서버를 시작한 뒤에 추가한 업체)"
        ))
    })
}

fn provider_view(state: &ApiState, record: ProviderRecord) -> ProviderView {
    let runtime =
        runtime(state, &record.id).map(|provider| RuntimeView::from(provider.runtime_status()));
    ProviderView {
        id: record.id,
        display_name: record.display_name,
        adapter_kind: record.adapter_kind,
        base_url: record.base_url,
        enabled: record.enabled,
        updated_at: record.updated_at,
        runtime,
    }
}

/// 개수를 API 타입(`u32`)으로 바꾼다. 업체 하나의 동시 실행 수가 40억을 넘을 일은 없다.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl From<RuntimeStatus> for RuntimeView {
    fn from(status: RuntimeStatus) -> Self {
        let circuit = status.circuit;
        Self {
            circuit: CircuitView {
                state: match circuit.state {
                    CircuitState::Closed => CircuitStateView::Closed,
                    CircuitState::Open => CircuitStateView::Open,
                    CircuitState::HalfOpen => CircuitStateView::HalfOpen,
                },
                forced: circuit.forced.map(|forced| match forced {
                    ForcedState::Open => ForcedStateView::Open,
                    ForcedState::Closed => ForcedStateView::Closed,
                }),
                recent_calls: circuit.recent_calls,
                recent_failures: circuit.recent_failures,
                open_until: circuit.open_until.map(to_utc),
                probes_succeeded: circuit.probes_succeeded,
                probes_in_flight: circuit.probes_in_flight,
            },
            paused_until: status.paused_until.map(to_utc),
            in_flight: count(status.in_flight),
            max_concurrency: count(status.max_concurrency),
            limiter_available: status.limiter_available,
            limiter_burst: status.limiter_burst,
            rate_limit_allowed: status.rate_limit_allowed,
            rate_limit_ceiling: status.rate_limit_ceiling,
            rate_limit_remaining: status.rate_limit_remaining,
        }
    }
}

/// 정책 → 응답. 필드를 `..` 없이 모두 풀어 쓴다: 정책에 필드가 늘면 여기서 컴파일 에러가 나서 화면에 빠뜨리지 않는다.
impl From<&ProviderPolicy> for PolicyView {
    fn from(policy: &ProviderPolicy) -> Self {
        let ProviderPolicy {
            attempt_timeout_ms,
            call_budget_ms,
            retry,
            rate_limit,
            bulkhead,
            circuit_breaker,
            availability_cache_ttl_ms,
        } = policy;
        let RetryPolicy {
            max_attempts,
            base_delay_ms,
            max_delay_ms,
        } = *retry;
        let RateLimitPolicy {
            per_second,
            burst,
            max_wait_ms: rate_limit_wait,
            adaptive,
        } = *rate_limit;
        let BulkheadPolicy {
            max_concurrency,
            max_wait_ms: bulkhead_wait,
        } = *bulkhead;
        let CircuitBreakerPolicy {
            window,
            min_calls,
            failure_rate,
            open_ms,
            half_open_probes,
        } = *circuit_breaker;
        Self {
            attempt_timeout_ms: *attempt_timeout_ms,
            call_budget_ms: *call_budget_ms,
            retry: RetryPolicyView {
                max_attempts,
                base_delay_ms,
                max_delay_ms,
            },
            rate_limit: RateLimitPolicyView {
                per_second,
                burst,
                max_wait_ms: rate_limit_wait,
                adaptive,
            },
            bulkhead: BulkheadPolicyView {
                max_concurrency,
                max_wait_ms: bulkhead_wait,
            },
            circuit_breaker: CircuitBreakerPolicyView {
                window,
                min_calls,
                failure_rate,
                open_ms,
                half_open_probes,
            },
            availability_cache_ttl_ms: *availability_cache_ttl_ms,
        }
    }
}

/// 정책 변경 요청 → 정책. 이쪽도 `..` 없이 풀어 써서, 요청 모양과 정책이 어긋나면 컴파일 에러가 난다.
impl From<PolicyView> for ProviderPolicy {
    fn from(view: PolicyView) -> Self {
        let PolicyView {
            attempt_timeout_ms,
            call_budget_ms,
            retry,
            rate_limit,
            bulkhead,
            circuit_breaker,
            availability_cache_ttl_ms,
        } = view;
        Self {
            attempt_timeout_ms,
            call_budget_ms,
            retry: RetryPolicy {
                max_attempts: retry.max_attempts,
                base_delay_ms: retry.base_delay_ms,
                max_delay_ms: retry.max_delay_ms,
            },
            rate_limit: RateLimitPolicy {
                per_second: rate_limit.per_second,
                burst: rate_limit.burst,
                max_wait_ms: rate_limit.max_wait_ms,
                adaptive: rate_limit.adaptive,
            },
            bulkhead: BulkheadPolicy {
                max_concurrency: bulkhead.max_concurrency,
                max_wait_ms: bulkhead.max_wait_ms,
            },
            circuit_breaker: CircuitBreakerPolicy {
                window: circuit_breaker.window,
                min_calls: circuit_breaker.min_calls,
                failure_rate: circuit_breaker.failure_rate,
                open_ms: circuit_breaker.open_ms,
                half_open_probes: circuit_breaker.half_open_probes,
            },
            availability_cache_ttl_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 정책과_요청_모양은_왕복해도_같다() {
        let policy = ProviderPolicy {
            attempt_timeout_ms: 1234,
            rate_limit: RateLimitPolicy {
                adaptive: false,
                ..RateLimitPolicy::default()
            },
            availability_cache_ttl_ms: 5_000,
            ..ProviderPolicy::default()
        };
        assert_eq!(ProviderPolicy::from(PolicyView::from(&policy)), policy);
    }
}
