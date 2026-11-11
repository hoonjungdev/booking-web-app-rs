//! `GET /v1/providers`, `GET /v1/providers/{provider}/availability` (docs/04-api.md §2 빈 시간 조회)
//!
//! 빈 시간은 업체에 원본이 있으므로 우리 DB에 저장하지 않는다. 요청이 오면 업체에 묻고 짧게 캐시한다
//! (`crate::availability`: TTL 30초, 업체가 실패하면 5분 안의 예전 값을 `stale: true`로).
//! 업체 호출은 예약 제출과 같은 장애 대응 파이프라인(서킷 · rate limit · bulkhead)을 거친다.

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use chrono::{DateTime, NaiveDate, Utc};
use domain::{ProviderId, RejectReason};
use providers::ProviderError;
use serde::{Deserialize, Serialize};
use tracing::{Span, field::Empty};
use utoipa::ToSchema;

use super::{ApiError, ApiState, Problem, docs::tags};
use crate::auth::AuthenticatedClient;

/// 자원 ID의 최대 길이 (예약 요청과 같은 규칙)
const MAX_RESOURCE_ID_LEN: usize = 100;

/// 예약을 받는 업체
#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderSummary {
    pub id: ProviderId,
    pub display_name: String,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AvailabilityQuery {
    /// 업체 안의 예약 대상 (매장 등). 형식은 업체마다 다르다 (alpha: `store-001`, beta: `GN01`)
    resource_id: String,
    /// 업체의 현지 날짜 (`YYYY-MM-DD`). 로컬 mock 업체는 한국 날짜다
    date: NaiveDate,
}

/// 빈 시간 조회 결과
#[derive(Debug, Serialize, ToSchema)]
pub struct AvailabilityResponse {
    pub provider: ProviderId,
    pub resource_id: String,
    pub date: NaiveDate,
    pub slots: Vec<SlotView>,
    /// 업체에서 이 값을 가져온 시각
    pub fetched_at: DateTime<Utc>,
    /// 업체 호출이 실패해 예전 값(최대 5분 전)을 돌려줬다
    pub stale: bool,
}

/// 예약할 수 있는 시간 한 칸
#[derive(Debug, Serialize, ToSchema)]
pub struct SlotView {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// 남은 자리 (0이면 마감)
    pub remaining: u32,
}

/// 예약을 받는 업체 목록
#[utoipa::path(
    get,
    path = "/v1/providers",
    operation_id = "list_providers",
    tag = tags::BOOKINGS,
    security(("api_key" = [])),
    responses(
        (status = 200, description = "사용 중인 업체", body = Vec<ProviderSummary>),
        (status = 401, description = "API 키가 없거나 틀림", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "providers.list", skip_all)]
pub async fn list_providers(
    State(state): State<ApiState>,
    _client: AuthenticatedClient,
) -> Result<Json<Vec<ProviderSummary>>, ApiError> {
    let providers = db::providers::list_enabled(&state.pool).await?;
    Ok(Json(
        providers
            .into_iter()
            .map(|provider| ProviderSummary {
                id: provider.id,
                display_name: provider.display_name,
            })
            .collect(),
    ))
}

/// 자원 하나의 날짜별 빈 시간
///
/// 업체 결과를 업체 정책의 시간(기본 30초) 동안 캐시한다. 업체 호출이 실패해도 5분 안에 가져온 값이 있으면
/// `"stale": true`로 돌려준다 (stale-if-error).
#[utoipa::path(
    get,
    path = "/v1/providers/{provider}/availability",
    operation_id = "get_availability",
    tag = tags::BOOKINGS,
    security(("api_key" = [])),
    params(("provider" = String, Path, description = "업체 ID"), AvailabilityQuery),
    responses(
        (status = 200, description = "빈 시간", body = AvailabilityResponse),
        (status = 400, description = "조건 값 오류 (자원 ID · 날짜 형식)", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "API 키가 없거나 틀림", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "없는 업체 · 업체에 없는 자원", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "예약을 받지 않는 업체", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "업체를 부를 수 없고 캐시도 없음 · 이 프로세스에 업체 파이프라인이 없음", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "availability.get", skip_all, fields(provider = %provider, cache = Empty, stale = Empty))]
pub async fn availability(
    State(state): State<ApiState>,
    _client: AuthenticatedClient,
    Path(provider): Path<String>,
    query: Result<Query<AvailabilityQuery>, QueryRejection>,
) -> Result<Json<AvailabilityResponse>, ApiError> {
    let Query(query) = query?;
    let resource_id = query.resource_id.trim();
    if resource_id.is_empty() || resource_id.chars().count() > MAX_RESOURCE_ID_LEN {
        return Err(ApiError::InvalidRequest(format!(
            "resource_id는 공백이 아닌 1~{MAX_RESOURCE_ID_LEN}자여야 합니다"
        )));
    }
    let not_found = || ApiError::ProviderNotFound(provider.clone());
    let provider_id = ProviderId::parse(&provider).map_err(|_| not_found())?;
    let record = db::providers::find(&state.pool, &provider_id)
        .await?
        .ok_or_else(not_found)?;
    if !record.enabled {
        return Err(ApiError::ProviderDisabled(record.id.to_string()));
    }
    // 업체 파이프라인(서킷 · rate limit 상태)은 워커와 같은 프로세스에만 있다 (docs/decisions.md D-10)
    let pipeline = state
        .providers
        .as_ref()
        .and_then(|registry| registry.get(&record.id))
        .ok_or(ApiError::ProviderRuntimeUnavailable)?;

    let result = state
        .availability
        .get(&pipeline, resource_id, query.date)
        .await
        .map_err(|error| match error {
            ProviderError::Rejected {
                reason: RejectReason::ResourceNotFound,
            } => ApiError::ResourceNotFound(resource_id.to_owned()),
            ProviderError::Rejected { reason } => ApiError::InvalidRequest(format!(
                "업체가 조회 조건을 받지 않았습니다 ({})",
                reason.code()
            )),
            // 오류 메시지(`Display`)에는 업체가 보낸 글자가 없다 (ProviderError 규칙)
            error => ApiError::ProviderUnavailable(error.to_string()),
        })?;
    let span = Span::current();
    span.record("cache", result.source.as_str());
    span.record("stale", result.stale);

    Ok(Json(AvailabilityResponse {
        provider: record.id,
        resource_id: resource_id.to_owned(),
        date: query.date,
        slots: result
            .slots
            .iter()
            .map(|slot| SlotView {
                start: slot.start,
                end: slot.end,
                remaining: slot.remaining,
            })
            .collect(),
        fetched_at: result.fetched_at,
        stale: result.stale,
    }))
}
