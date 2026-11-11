//! API 에러와 RFC 9457 Problem Details 응답 (≈ ASP.NET Core의 `ProblemDetails`)

use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("API 키가 없거나 올바르지 않습니다")]
    Unauthorized,
    #[error("{0}")]
    InvalidRequest(String),
    #[error("Idempotency-Key 헤더가 필요합니다 (출력 가능한 ASCII 1~255자)")]
    IdempotencyKeyRequired,
    #[error("이 Idempotency-Key는 다른 요청 본문으로 이미 사용되었습니다")]
    IdempotencyKeyReused,
    #[error("업체를 찾을 수 없습니다: {0}")]
    ProviderNotFound(String),
    #[error("현재 예약을 받지 않는 업체입니다: {0}")]
    ProviderDisabled(String),
    #[error("예약을 찾을 수 없습니다")]
    BookingNotFound,
    #[error("작업을 찾을 수 없습니다")]
    JobNotFound,
    #[error("없는 경로입니다: {0}")]
    RouteNotFound(String),
    /// 지금 상태에서는 할 수 없는 조작 (예: `failed`가 아닌 예약 재시도, dead가 아닌 작업 재큐잉)
    #[error("{0}")]
    Conflict(String),
    /// 업체 파이프라인(서킷 · rate limit 상태)이 이 프로세스에 없다. 워커 없이 `--role api`로 띄운 경우다.
    #[error(
        "이 서버 프로세스에는 업체 파이프라인이 없습니다 (워커와 함께 실행해야 합니다: --role all)"
    )]
    ProviderRuntimeUnavailable,
    #[error("장애 주입 API가 꺼져 있습니다 (BOOKING_CHAOS_URL을 설정하세요)")]
    ChaosDisabled,
    /// 중계하는 외부 API(mock 업체의 장애 주입 API)가 실패했다
    #[error("{0}")]
    Upstream(String),
    #[error(transparent)]
    Db(#[from] db::DbError),
    #[error("{0}")]
    Internal(String),
}

impl ApiError {
    /// (HTTP 상태, 문제 종류 식별자, 제목)
    fn describe(&self) -> (StatusCode, &'static str, &'static str) {
        match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", "인증 실패"),
            Self::InvalidRequest(_) => (
                StatusCode::BAD_REQUEST,
                "invalid-request",
                "요청 값이 올바르지 않습니다",
            ),
            Self::IdempotencyKeyRequired => (
                StatusCode::BAD_REQUEST,
                "idempotency-key-required",
                "Idempotency-Key 헤더가 필요합니다",
            ),
            Self::IdempotencyKeyReused => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency-key-reused",
                "같은 Idempotency-Key로 다른 요청이 들어왔습니다",
            ),
            Self::ProviderNotFound(_) => (
                StatusCode::NOT_FOUND,
                "provider-not-found",
                "업체를 찾을 수 없습니다",
            ),
            Self::ProviderDisabled(_) => (
                StatusCode::CONFLICT,
                "provider-disabled",
                "예약을 받지 않는 업체입니다",
            ),
            Self::BookingNotFound => (
                StatusCode::NOT_FOUND,
                "booking-not-found",
                "예약을 찾을 수 없습니다",
            ),
            Self::JobNotFound => (
                StatusCode::NOT_FOUND,
                "job-not-found",
                "작업을 찾을 수 없습니다",
            ),
            Self::RouteNotFound(_) => (StatusCode::NOT_FOUND, "not-found", "없는 경로입니다"),
            Self::Conflict(_) => (
                StatusCode::CONFLICT,
                "conflict",
                "지금 상태에서는 할 수 없는 요청입니다",
            ),
            Self::ProviderRuntimeUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "provider-runtime-unavailable",
                "업체 파이프라인을 쓸 수 없습니다",
            ),
            Self::ChaosDisabled => (
                StatusCode::SERVICE_UNAVAILABLE,
                "chaos-disabled",
                "장애 주입 API를 쓸 수 없습니다",
            ),
            Self::Upstream(_) => (
                StatusCode::BAD_GATEWAY,
                "upstream-error",
                "중계한 API가 실패했습니다",
            ),
            Self::Db(_) | Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                "서버 내부 오류",
            ),
        }
    }
}

/// RFC 9457 Problem Details. 모든 오류 응답의 본문이다 (`application/problem+json`).
#[derive(Debug, Serialize, ToSchema)]
pub struct Problem {
    /// 문제 종류를 나타내는 URI (`https://booking.local/problems/<종류>`). 클라이언트는 이 값으로 오류를 구분한다.
    /// `type`은 Rust 예약어라서 필드 이름을 바꾸고 JSON에서만 `type`으로 내보낸다.
    #[serde(rename = "type")]
    #[schema(example = "https://booking.local/problems/booking-not-found")]
    kind: String,
    /// 사람이 읽을 짧은 제목
    title: &'static str,
    /// HTTP 상태 코드
    status: u16,
    /// 이번 요청에 대한 자세한 설명
    detail: String,
    /// 이 요청의 trace ID. 문의를 받으면 이 값으로 Grafana에서 요청의 처리 과정을 찾는다.
    /// trace가 없으면(OpenTelemetry를 설치하지 않은 테스트 등) 필드를 아예 넣지 않는다.
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_id: Option<String>,
}

/// 핸들러가 `Err(ApiError)`를 돌려주면 axum이 이 구현으로 응답을 만든다.
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, slug, title) = self.describe();
        let detail = if status.is_server_error() && !self.is_expected_unavailable() {
            // 내부 오류의 자세한 내용은 로그에만 남기고 클라이언트에는 알리지 않는다
            tracing::error!(error = %self, "요청 처리 중 서버 오류");
            "요청을 처리하는 중 서버에 문제가 생겼습니다".to_owned()
        } else {
            self.to_string()
        };
        let problem = Problem {
            kind: format!("https://booking.local/problems/{slug}"),
            title,
            status: status.as_u16(),
            detail,
            // 핸들러(와 인증 추출기)는 요청 span 안에서 실행되므로, 지금 span의 trace가 곧 이 요청의 trace다
            trace_id: telemetry::current_trace_id(),
        };
        (
            status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            Json(problem),
        )
            .into_response()
    }
}

impl ApiError {
    /// 5xx지만 서버 내부 문제가 아니라 "설정상 쓸 수 없음 · 중계 대상 실패"라서 설명을 그대로 보여 줘도 되는 오류.
    /// 관리자 화면이 왜 안 되는지 알려 주려면 설명이 필요하다.
    fn is_expected_unavailable(&self) -> bool {
        matches!(
            self,
            Self::ProviderRuntimeUnavailable | Self::ChaosDisabled | Self::Upstream(_)
        )
    }
}

/// 트랜잭션 시작 · 커밋처럼 sqlx를 직접 쓰는 곳의 에러도 `?`로 넘길 수 있게 한다
impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(error.into())
    }
}

/// 요청을 해석하지 못한 경우(쿼리 · 경로 · JSON 본문)를 Problem Details 형식의 400으로 돌려준다.
/// axum이 기본으로 돌려주는 거절은 일반 텍스트라 형식이 맞지 않는다.
/// 핸들러에서 `Result<Query<T>, QueryRejection>`으로 받아 `query?`라고 쓰면 이 변환이 적용된다.
impl From<axum::extract::rejection::QueryRejection> for ApiError {
    fn from(rejection: axum::extract::rejection::QueryRejection) -> Self {
        Self::InvalidRequest(rejection.body_text())
    }
}

impl From<axum::extract::rejection::PathRejection> for ApiError {
    fn from(rejection: axum::extract::rejection::PathRejection) -> Self {
        Self::InvalidRequest(rejection.body_text())
    }
}

impl From<axum::extract::rejection::JsonRejection> for ApiError {
    fn from(rejection: axum::extract::rejection::JsonRejection) -> Self {
        Self::InvalidRequest(rejection.body_text())
    }
}
