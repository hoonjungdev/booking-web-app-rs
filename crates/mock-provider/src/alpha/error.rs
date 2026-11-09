//! alpha API 에러와 HTTP 응답 변환

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// alpha 업체가 돌려주는 업무 에러.
///
/// `thiserror`의 `#[error("...")]`는 사람이 읽을 메시지(`Display`)를 자동으로 만들어 준다.
/// (≈ C#에서 Exception 하위 클래스마다 Message를 정의하는 것. 단, Rust는 throw 대신 `Result`로 돌려준다)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AlphaError {
    #[error("자원을 찾을 수 없습니다: {0}")]
    ResourceNotFound(String),
    #[error("예약을 찾을 수 없습니다: {0}")]
    ReservationNotFound(String),
    #[error("요청이 올바르지 않습니다: {0}")]
    InvalidRequest(String),
    #[error("존재하지 않는 슬롯입니다")]
    InvalidSlot,
    #[error("이미 지난 슬롯입니다")]
    SlotInPast,
    #[error("슬롯이 마감되었습니다")]
    SlotUnavailable,
    #[error("같은 Idempotency-Key로 다른 요청이 들어왔습니다")]
    IdempotencyKeyMismatch,
}

impl AlphaError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::ResourceNotFound(_) | Self::ReservationNotFound(_) => StatusCode::NOT_FOUND,
            Self::InvalidRequest(_) | Self::InvalidSlot | Self::SlotInPast => {
                StatusCode::BAD_REQUEST
            }
            Self::SlotUnavailable => StatusCode::CONFLICT,
            Self::IdempotencyKeyMismatch => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }

    /// 응답 본문의 기계용 코드
    pub fn code(&self) -> &'static str {
        match self {
            Self::ResourceNotFound(_) => "resource_not_found",
            Self::ReservationNotFound(_) => "reservation_not_found",
            Self::InvalidRequest(_) => "invalid_request",
            Self::InvalidSlot => "invalid_slot",
            Self::SlotInPast => "slot_in_past",
            Self::SlotUnavailable => "slot_unavailable",
            Self::IdempotencyKeyMismatch => "idempotency_key_mismatch",
        }
    }
}

/// 핸들러가 `Err(AlphaError)`를 돌려주면 axum이 이 구현으로 HTTP 응답을 만든다.
impl IntoResponse for AlphaError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.code(), "message": self.to_string() } });
        (self.status(), Json(body)).into_response()
    }
}
