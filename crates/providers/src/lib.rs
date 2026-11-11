//! 외부 업체 어댑터와 장애 대응 파이프라인
//!
//! | 모듈 | 역할 |
//! |---|---|
//! | `alpha` | alpha 업체 어댑터: "업체 HTTP API ↔ 우리 도메인 타입" 변환과 응답 분류([`ProviderError`])만 맡는다 |
//! | `error` | 실패 분류와 분류별 판단 (재시도 안전성, 서킷 집계) |
//! | `policy` | 업체별 정책 (`providers.policy` jsonb) |
//! | [`resilience`] | 어댑터를 감싸는 [`ResilientProvider`]: 재시도 · 타임아웃 · 서킷 브레이커 · rate limit · bulkhead |
//! | `recorder` | 시도마다 남기는 호출 기록을 채널로 내보냄 (DB 저장은 booking-server) |
//! | `metrics` | 업체 호출 지표 (시도 시간, 재시도, 호출 제한) |
//! | `trace` | 업체 요청에 trace 정보(`traceparent`)를 실어 보냄 |

mod alpha;
mod error;
mod metrics;
mod policy;
mod recorder;
pub mod resilience;
mod trace;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use domain::{Booking, BookingId, Customer};

pub use alpha::AlphaAdapter;
pub use error::{CircuitVerdict, ProviderError};
pub use policy::{
    BulkheadPolicy, CircuitBreakerPolicy, ProviderPolicy, RateLimitPolicy, RetryPolicy,
};
pub use recorder::{CallRecord, CallRecorder, call_recorder};
pub use resilience::{
    CallContext, CircuitAction, CircuitSnapshot, CircuitState, ForcedState, Operation,
    ResilientProvider, RuntimeStatus,
};

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

/// 응답에서 읽은 부가 정보 (호출 기록용). 응답을 받지 못했으면 모두 `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResponseMeta {
    pub http_status: Option<u16>,
    /// 업체가 알려 준 남은 호출 수 (`X-RateLimit-Remaining`)
    pub rate_limit_remaining: Option<u32>,
}

/// 어댑터 호출 한 번의 결과와 응답 부가 정보.
///
/// 결과(`result`)만으로는 HTTP 상태나 rate limit 헤더를 알 수 없어서 함께 돌려준다. `ResilientProvider`가
/// 부가 정보는 호출 기록에 남기고 결과만 바깥에 돌려준다. (docs/decisions.md D-17)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterReply<T> {
    pub result: Result<T, ProviderError>,
    pub meta: ResponseMeta,
}

/// `AdapterReply::from(Ok(..))`처럼 부가 정보 없이 결과만으로 만든다 (테스트용 가짜 어댑터, 타임아웃 등)
impl<T> From<Result<T, ProviderError>> for AdapterReply<T> {
    fn from(result: Result<T, ProviderError>) -> Self {
        Self {
            result,
            meta: ResponseMeta::default(),
        }
    }
}

/// 외부 업체 어댑터 (≈ C#의 `interface IProviderAdapter`).
///
/// 업체마다 구현이 다르고(alpha, beta …) 실행 중에 업체 ID로 골라 써야 해서 `Arc<dyn ProviderAdapter>`로 담는다.
/// `dyn Trait`(≈ 인터페이스 타입 변수)으로 쓰는 trait에는 아직 `async fn`을 그대로 넣을 수 없어서
/// `#[async_trait]`이 `async fn`을 `Box`에 담긴 Future를 돌려주는 함수로 바꿔 준다.
///
/// 어댑터는 한 번만 보낸다. 재시도 · 타임아웃 · 서킷은 바깥의 [`ResilientProvider`]가 맡는다.
#[async_trait::async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    async fn create_booking(&self, request: &CreateBooking) -> AdapterReply<ProviderBooking>;

    /// 우리 참조값(예약 ID)으로 업체의 **확정된** 예약을 찾는다. 없으면 `Ok(None)`.
    /// `capabilities().lookup_by_reference`가 `false`인 어댑터는 부르지 않는다.
    async fn find_booking(&self, reference: BookingId) -> AdapterReply<Option<ProviderBooking>>;
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("알 수 없는 어댑터 종류입니다: {0}")]
    UnknownKind(String),
    #[error("업체 정책 형식이 올바르지 않습니다: {0}")]
    Policy(#[from] serde_json::Error),
    #[error("업체 정책 값이 올바르지 않습니다: {0}")]
    InvalidPolicy(String),
    #[error("HTTP 클라이언트를 만들 수 없습니다: {0}")]
    Http(#[from] reqwest::Error),
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
