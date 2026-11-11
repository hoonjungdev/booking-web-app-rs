//! beta API 에러와 HTTP 응답 변환
//!
//! 레거시 업체라서 업무 에러도 **HTTP 200**으로 돌려준다. 실패는 본문의 `ok: false`와 `code`로만 알 수 있다:
//! `{"ok": false, "code": "FULL", "message": "남은 자리가 없습니다"}`

use axum::{
    Json,
    extract::rejection::{JsonRejection, PathRejection, QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// beta 업체가 돌려주는 업무 에러 (`code`는 [`BetaError::code`])
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BetaError {
    /// 요청 값이 틀렸거나 요청을 해석할 수 없음
    #[error("요청이 올바르지 않습니다: {0}")]
    BadRequest(String),
    #[error("매장을 찾을 수 없습니다: {0}")]
    NoShop(String),
    /// `startAt`이 `yyyyMMddHHmm`이 아니거나 슬롯 시작 시각이 아님
    #[error(
        "예약할 수 없는 시각입니다 (startAt은 yyyyMMddHHmm 형식의 슬롯 시작 시각이어야 합니다)"
    )]
    BadTime,
    #[error("이미 지난 시각입니다")]
    Past,
    #[error("남은 자리가 없습니다")]
    Full,
    #[error("예약을 찾을 수 없습니다: {0}")]
    NotFound(String),
    #[error("이미 취소된 예약입니다")]
    AlreadyCancelled,
}

impl BetaError {
    /// 응답 본문의 기계용 코드
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::NoShop(_) => "NO_SHOP",
            Self::BadTime => "BAD_TIME",
            Self::Past => "PAST",
            Self::Full => "FULL",
            Self::NotFound(_) => "NOT_FOUND",
            Self::AlreadyCancelled => "ALREADY_CANCELLED",
        }
    }
}

/// 핸들러가 `Err(BetaError)`를 돌려주면 axum이 이 구현으로 HTTP 응답을 만든다. 상태 코드는 언제나 200이다.
impl IntoResponse for BetaError {
    fn into_response(self) -> Response {
        let body = json!({ "ok": false, "code": self.code(), "message": self.to_string() });
        (StatusCode::OK, Json(body)).into_response()
    }
}

/// 요청을 해석하지 못한 경우(JSON 본문 · 쿼리 · 경로)도 이 업체 형식(HTTP 200 + `BAD_REQUEST`)으로 돌려준다.
/// axum이 기본으로 돌려주는 거절은 400/422 + 일반 텍스트라서 형식이 맞지 않는다.
/// 핸들러에서 `Result<Json<T>, JsonRejection>`으로 받아 `body?`라고 쓰면 `?`가 이 변환으로 에러 타입을 맞춘다.
impl From<JsonRejection> for BetaError {
    fn from(rejection: JsonRejection) -> Self {
        Self::BadRequest(rejection.body_text())
    }
}

impl From<QueryRejection> for BetaError {
    fn from(rejection: QueryRejection) -> Self {
        Self::BadRequest(rejection.body_text())
    }
}

impl From<PathRejection> for BetaError {
    fn from(rejection: PathRejection) -> Self {
        Self::BadRequest(rejection.body_text())
    }
}
