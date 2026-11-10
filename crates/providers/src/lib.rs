//! 외부 업체 어댑터와 장애 대응 파이프라인
//!
//! 어댑터는 "업체 HTTP API ↔ 우리 도메인 타입" 변환과 응답 분류([`ProviderError`])만 맡는다.
//! 재시도 · 서킷 브레이커 · rate limit은 어댑터를 감싸는 `ResilientProvider`가 맡는다 (Phase 3).
//! Phase 2에서는 호출 타임아웃만 적용한다.

mod alpha;
mod error;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use domain::{Booking, BookingId, Customer};
use serde::Deserialize;

pub use alpha::AlphaAdapter;
pub use error::ProviderError;

/// 어댑터가 할 수 있는 일 (docs/03-resilience.md §5)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// 생성 · 취소에 멱등키를 지원한다 → 타임아웃 뒤 같은 키로 재시도해도 중복 생성이 없다
    pub idempotency_key: bool,
    /// 우리 참조값으로 업체 예약을 조회할 수 있다
    pub lookup_by_reference: bool,
    /// 기간별 예약 목록을 받을 수 있다 (대사용)
    pub list_bookings: bool,
}

/// 업체에 보낼 예약 생성 요청
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateBooking {
    /// 우리 예약 ID. 업체 쪽 참조값과 멱등키로 쓴다.
    pub reference: BookingId,
    pub resource_id: String,
    pub slot_start: DateTime<Utc>,
    pub party_size: u32,
    pub customer: Customer,
}

impl CreateBooking {
    pub fn from_booking(booking: &Booking) -> Self {
        Self {
            reference: booking.id,
            resource_id: booking.request.resource_id.clone(),
            slot_start: booking.request.slot_start,
            party_size: booking.request.party_size,
            customer: booking.request.customer.clone(),
        }
    }
}

/// 업체가 확정한 예약
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderBooking {
    pub provider_booking_id: String,
}

/// 외부 업체 어댑터 (≈ C#의 `interface IProviderAdapter`).
///
/// 업체마다 구현이 다르고(alpha, beta …) 실행 중에 업체 ID로 골라 써야 해서 `Arc<dyn ProviderAdapter>`로 담는다.
/// `dyn Trait`(≈ 인터페이스 타입 변수)으로 쓰는 trait에는 아직 `async fn`을 그대로 넣을 수 없어서
/// `#[async_trait]`이 `async fn`을 `Box`에 담긴 Future를 돌려주는 함수로 바꿔 준다.
#[async_trait::async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    async fn create_booking(
        &self,
        request: &CreateBooking,
    ) -> Result<ProviderBooking, ProviderError>;
}

/// 업체별 정책 (`providers.policy` jsonb). Phase 3에서 재시도 · rate limit · 서킷 설정이 추가된다.
///
/// `#[serde(default)]`: JSON에 없는 필드는 `Default` 값으로 채운다. 모르는 필드는 무시한다(나중에 추가될 설정).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ProviderPolicy {
    /// 호출 한 번의 최대 시간
    pub attempt_timeout_ms: u64,
}

impl Default for ProviderPolicy {
    fn default() -> Self {
        Self {
            attempt_timeout_ms: 3000,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("알 수 없는 어댑터 종류입니다: {0}")]
    UnknownKind(String),
    #[error("업체 정책 형식이 올바르지 않습니다: {0}")]
    Policy(#[from] serde_json::Error),
    #[error("HTTP 클라이언트를 만들 수 없습니다: {0}")]
    Http(#[from] reqwest::Error),
}

impl ProviderPolicy {
    pub fn from_json(value: serde_json::Value) -> Result<Self, BuildError> {
        Ok(serde_json::from_value(value)?)
    }
}

/// `providers.adapter_kind`에 맞는 어댑터를 만든다
pub fn build_adapter(
    kind: &str,
    base_url: &str,
    policy: &ProviderPolicy,
) -> Result<Arc<dyn ProviderAdapter>, BuildError> {
    match kind {
        "alpha_v1" => Ok(Arc::new(AlphaAdapter::new(base_url, policy)?)),
        other => Err(BuildError::UnknownKind(other.to_owned())),
    }
}
