//! API 에러와 RFC 9457 Problem Details 응답 (≈ ASP.NET Core의 `ProblemDetails`)

use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

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
            Self::Db(_) | Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                "서버 내부 오류",
            ),
        }
    }
}

#[derive(Serialize)]
struct Problem {
    /// `type`은 Rust 예약어라서 필드 이름을 바꾸고 JSON에서만 `type`으로 내보낸다
    #[serde(rename = "type")]
    kind: String,
    title: &'static str,
    status: u16,
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
        let detail = if status.is_server_error() {
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
