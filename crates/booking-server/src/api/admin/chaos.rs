//! 관리자: mock 업체 장애 주입 중계 (개발 환경 전용, docs/04-api.md §3)
//!
//! `BOOKING_CHAOS_URL`을 설정해야 쓸 수 있다. 실제 업체에는 이런 API가 없으므로 운영 환경에서는 설정하지 않는다.

use axum::{
    Json,
    extract::{
        Path, State,
        rejection::{JsonRejection, PathRejection},
    },
};
use domain::ProviderId;

use crate::{
    api::{ApiError, ApiState, Problem, docs::tags},
    chaos::{ChaosClient, ChaosError, ChaosPreset, ChaosSettings},
};

/// 업체의 지금 장애 설정
#[utoipa::path(
    get,
    path = "/chaos/{provider}",
    operation_id = "admin_get_chaos",
    tag = tags::ADMIN_CHAOS,
    params(("provider" = String, Path, description = "업체 ID (mock 업체 이름과 같다)")),
    responses(
        (status = 200, description = "장애 설정", body = ChaosSettings),
        (status = 404, description = "mock 업체에 없는 업체", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "mock 업체를 부를 수 없음", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "장애 주입이 꺼져 있음 (BOOKING_CHAOS_URL)", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.chaos.get", skip_all, fields(provider = %provider))]
pub async fn get_chaos(
    State(state): State<ApiState>,
    Path(provider): Path<String>,
) -> Result<Json<ChaosSettings>, ApiError> {
    let (client, provider) = target(&state, &provider)?;
    Ok(Json(client.get(&provider).await.map_err(api_error)?))
}

/// 장애 설정을 바꾼다 (전체 교체)
#[utoipa::path(
    put,
    path = "/chaos/{provider}",
    operation_id = "admin_put_chaos",
    tag = tags::ADMIN_CHAOS,
    params(("provider" = String, Path, description = "업체 ID")),
    request_body = ChaosSettings,
    responses(
        (status = 200, description = "바뀐 장애 설정", body = ChaosSettings),
        (status = 400, description = "설정 값 오류 (확률이 0~1 밖 등)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "mock 업체에 없는 업체", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "mock 업체를 부를 수 없음", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "장애 주입이 꺼져 있음", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.chaos.put", skip_all, fields(provider = %provider))]
pub async fn put_chaos(
    State(state): State<ApiState>,
    Path(provider): Path<String>,
    body: Result<Json<ChaosSettings>, JsonRejection>,
) -> Result<Json<ChaosSettings>, ApiError> {
    let Json(settings) = body?;
    let (client, provider) = target(&state, &provider)?;
    // 보내기 전에 우리가 검사한다. 어느 값이 틀렸는지 우리 말로 알려 줄 수 있다.
    settings.validate().map_err(ApiError::InvalidRequest)?;
    let applied = client.put(&provider, &settings).await.map_err(api_error)?;
    tracing::info!(%provider, "관리자가 mock 업체의 장애 설정을 바꿨다");
    Ok(Json(applied))
}

/// 미리 정해 둔 장애 시나리오를 적용한다
#[utoipa::path(
    post,
    path = "/chaos/{provider}/preset/{name}",
    operation_id = "admin_apply_chaos_preset",
    tag = tags::ADMIN_CHAOS,
    params(
        ("provider" = String, Path, description = "업체 ID"),
        ("name" = ChaosPreset, Path, description = "시나리오 이름"),
    ),
    responses(
        (status = 200, description = "적용한 장애 설정", body = ChaosSettings),
        (status = 400, description = "없는 시나리오", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "mock 업체에 없는 업체", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "mock 업체를 부를 수 없음", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "장애 주입이 꺼져 있음", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.chaos.preset", skip_all)]
pub async fn apply_chaos_preset(
    State(state): State<ApiState>,
    path: Result<Path<(String, ChaosPreset)>, PathRejection>,
) -> Result<Json<ChaosSettings>, ApiError> {
    let Path((provider, preset)) = path?;
    let (client, provider) = target(&state, &provider)?;
    let applied = client
        .apply_preset(&provider, preset)
        .await
        .map_err(api_error)?;
    tracing::info!(%provider, ?preset, "관리자가 mock 업체에 장애 시나리오를 적용했다");
    Ok(Json(applied))
}

/// 장애 주입 클라이언트와 업체 ID. 꺼져 있거나 업체 ID 형식이 틀리면 에러.
fn target<'a>(
    state: &'a ApiState,
    provider: &str,
) -> Result<(&'a ChaosClient, ProviderId), ApiError> {
    let client = state.chaos.as_ref().ok_or(ApiError::ChaosDisabled)?;
    let provider =
        ProviderId::parse(provider).map_err(|_| ApiError::ProviderNotFound(provider.to_owned()))?;
    Ok((client, provider))
}

fn api_error(error: ChaosError) -> ApiError {
    match error {
        ChaosError::UnknownProvider(provider) => ApiError::ProviderNotFound(provider.to_string()),
        error @ ChaosError::Rejected => ApiError::InvalidRequest(error.to_string()),
        error @ (ChaosError::Unreachable(_) | ChaosError::Status(_)) => {
            tracing::warn!(%error, "장애 주입 API 중계 실패");
            ApiError::Upstream(error.to_string())
        }
    }
}
