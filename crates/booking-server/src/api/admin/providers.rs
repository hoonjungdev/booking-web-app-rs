//! 관리자: 업체 목록 · 상세(정책 · 런타임 상태) · 서킷 강제 조작

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
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

use super::to_utc;
use crate::api::{ApiError, ApiState, Problem, docs::tags};

/// 업체 한 곳 (목록의 한 줄)
#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderView {
    pub id: ProviderId,
    pub display_name: String,
    /// 어떤 어댑터 코드로 호출하는가 (`alpha_v1` …)
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
    /// 장애 대응 정책 (빠진 값은 기본값으로 채운 것). 서버가 시작할 때 읽어 적용한다
    pub policy: PolicyView,
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
    /// 우리 rate limiter 버킷에 남은 토큰 수 (추정값) / 버킷 크기
    pub limiter_available: u32,
    pub limiter_burst: u32,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CircuitActionView {
    /// 강제로 연다. 풀 때까지 호출하지 않는다 (업체 점검 공지에 대응)
    ForceOpen,
    /// 강제로 닫는다. 풀 때까지 실패가 쌓여도 열리지 않는다
    ForceClose,
    /// 강제 상태를 풀고 닫힌 상태에서 처음부터 다시 센다
    Reset,
}

/// 장애 대응 정책 (docs/03-resilience.md §3)
#[derive(Debug, Serialize, ToSchema)]
pub struct PolicyView {
    /// 시도 한 번의 최대 시간
    pub attempt_timeout_ms: u64,
    /// 호출 한 번(재시도 포함)에 쓸 수 있는 전체 시간
    pub call_budget_ms: u64,
    pub retry: RetryPolicyView,
    pub rate_limit: RateLimitPolicyView,
    pub bulkhead: BulkheadPolicyView,
    pub circuit_breaker: CircuitBreakerPolicyView,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RetryPolicyView {
    /// 첫 시도를 포함한 최대 시도 횟수
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RateLimitPolicyView {
    /// 초당 허용 호출 수
    pub per_second: u32,
    /// 한꺼번에 보낼 수 있는 호출 수 (버킷 크기)
    pub burst: u32,
    /// 토큰을 기다리는 최대 시간
    pub max_wait_ms: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BulkheadPolicyView {
    pub max_concurrency: u32,
    pub max_wait_ms: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CircuitBreakerPolicyView {
    /// 최근 몇 번의 결과로 실패율을 계산하는가
    pub window: u32,
    /// 이만큼은 호출해 봐야 판단한다
    pub min_calls: u32,
    /// 이 비율 이상 실패하면 연다 (0~1)
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
    responses((status = 200, description = "모든 업체 (사용하지 않는 업체 포함)", body = Vec<ProviderView>)),
)]
#[tracing::instrument(name = "admin.providers.list", skip_all)]
pub async fn list_providers(
    State(state): State<ApiState>,
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
    params(("id" = String, Path, description = "업체 ID")),
    responses(
        (status = 200, description = "업체 설정 · 정책 · 런타임 상태", body = ProviderDetail),
        (status = 404, description = "없는 업체", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.providers.get", skip_all)]
pub async fn get_provider(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<ProviderDetail>, ApiError> {
    let record = find(&state, &id).await?;
    let policy = ProviderPolicy::from_json(record.policy.clone()).map_err(|error| {
        ApiError::Internal(format!("업체 {id}의 정책을 읽을 수 없습니다: {error}"))
    })?;
    Ok(Json(ProviderDetail {
        policy: PolicyView::from(&policy),
        provider: provider_view(&state, record),
    }))
}

/// 서킷을 강제로 열거나 닫거나, 강제 상태를 푼다
///
/// 서킷 상태는 업체를 부르는 프로세스의 메모리에 있다. 워커와 같은 프로세스(`--role all`)에서만 조작할 수 있다.
#[utoipa::path(
    post,
    path = "/providers/{id}/circuit",
    operation_id = "admin_control_circuit",
    tag = tags::ADMIN_PROVIDERS,
    params(("id" = String, Path, description = "업체 ID")),
    request_body = CircuitCommand,
    responses(
        (status = 200, description = "조작한 뒤의 런타임 상태", body = RuntimeView),
        (status = 400, description = "요청 본문 오류", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "없는 업체", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "사용하지 않는 업체 (파이프라인이 없음)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "이 프로세스에 업체 파이프라인이 없음 (--role api)", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.providers.circuit", skip_all, fields(provider = %id))]
pub async fn control_circuit(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    body: Result<Json<CircuitCommand>, JsonRejection>,
) -> Result<Json<RuntimeView>, ApiError> {
    let Json(command) = body?;
    let provider = pipeline(&state, &id).await?;
    provider.control_circuit(match command.action {
        CircuitActionView::ForceOpen => CircuitAction::ForceOpen,
        CircuitActionView::ForceClose => CircuitAction::ForceClose,
        CircuitActionView::Reset => CircuitAction::Reset,
    });
    Ok(Json(RuntimeView::from(provider.runtime_status())))
}

/// DB의 업체 설정. 없으면 404.
async fn find(state: &ApiState, id: &str) -> Result<ProviderRecord, ApiError> {
    let not_found = || ApiError::ProviderNotFound(id.to_owned());
    let provider_id = ProviderId::parse(id).map_err(|_| not_found())?;
    db::providers::find(&state.pool, &provider_id)
        .await?
        .ok_or_else(not_found)
}

/// 이 프로세스의 업체 파이프라인. 없는 이유에 따라 다른 오류를 돌려준다.
async fn pipeline(state: &ApiState, id: &str) -> Result<Arc<ResilientProvider>, ApiError> {
    let record = find(state, id).await?;
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
    let runtime = state
        .providers
        .as_ref()
        .and_then(|registry| registry.get(&record.id))
        .map(|provider| RuntimeView::from(provider.runtime_status()));
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
        }
    }
}
