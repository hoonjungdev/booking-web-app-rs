//! 외부 업체 어댑터와 장애 대응 파이프라인
//!
//! | 모듈 | 역할 |
//! |---|---|
//! | `alpha` | alpha 업체 어댑터: "업체 HTTP API ↔ 우리 도메인 타입" 변환과 응답 분류([`ProviderError`])만 맡는다 |
//! | `beta` | beta 업체 어댑터: 시간대 변환(한국 시간 문자열), HTTP 200으로 오는 에러, 503 호출 제한 |
//! | `error` | 실패 분류와 분류별 판단 (재시도 안전성, 서킷 집계) |
//! | `policy` | 업체별 정책 (`providers.policy` jsonb) |
//! | [`resilience`] | 어댑터를 감싸는 [`ResilientProvider`]: 재시도 · 타임아웃 · 서킷 브레이커 · rate limit(AIMD) · bulkhead |
//! | `recorder` | 시도마다 남기는 호출 기록을 채널로 내보냄 (DB 저장은 booking-server) |
//! | `metrics` | 업체 호출 지표 (시도 시간, 재시도, 호출 제한) |
//! | `trace` | 업체 요청에 trace 정보(`traceparent`)를 실어 보냄 |

mod alpha;
mod beta;
mod error;
mod metrics;
mod policy;
mod recorder;
pub mod resilience;
mod trace;

use std::{sync::Arc, time::Duration};

use chrono::{DateTime, NaiveDate, Utc};
use domain::{Booking, BookingId, Customer};

pub use alpha::AlphaAdapter;
pub use beta::BetaAdapter;
pub use error::{CircuitVerdict, ProviderError};
pub use policy::{
    BulkheadPolicy, CircuitBreakerPolicy, MAX_ATTEMPT_TIMEOUT_MS, ProviderPolicy, RateLimitPolicy,
    RetryPolicy,
};
pub use recorder::{CallRecord, CallRecorder, call_recorder};
pub use resilience::{
    CallContext, CircuitAction, CircuitSnapshot, CircuitState, ForcedState, Operation,
    ResilientProvider, RuntimeStatus,
};

/// 어댑터 HTTP 클라이언트의 요청 타임아웃 (안전장치).
///
/// 시도 타임아웃은 [`ResilientProvider`]가 정책 값(`attempt_timeout_ms`)으로 건다. 정책은 실행 중에 바뀔 수 있으므로
/// (관리자 화면에서 편집) 어댑터를 만들 때 정책 값을 박아 두지 않고, 그보다 넉넉한 고정값을 안전장치로만 둔다.
/// 정책의 시도 타임아웃은 이 값을 넘을 수 없다 ([`MAX_ATTEMPT_TIMEOUT_MS`]).
pub const ADAPTER_TIMEOUT: Duration = Duration::from_millis(MAX_ATTEMPT_TIMEOUT_MS);

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

/// 업체의 기간별 예약 목록 한 건 (대사용, docs/03-resilience.md §9).
///
/// 업체가 함께 보내 준 고객 정보는 담지 않는다. 대사에는 필요 없고, 이 값은 이슈 기록 · 로그로 퍼져 나간다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedBooking {
    pub provider_booking_id: String,
    /// 우리가 보낸 참조값 (우리 예약 ID 문자열). 다른 경로로 만든 예약이면 없거나 다른 형식이다.
    pub reference: Option<String>,
    pub slot_start: DateTime<Utc>,
    pub status: ListedStatus,
}

/// 업체 쪽 예약 상태
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListedStatus {
    Confirmed,
    Cancelled,
}

/// 예약할 수 있는 시간 한 칸 (빈 시간 조회)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvailableSlot {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// 남은 자리 (0이면 마감)
    pub remaining: u32,
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
///
/// 몸통이 있는 메서드(취소 · 목록 · 빈 시간)는 **기본 구현**이다 (≈ C#의 default interface method).
/// 구현하지 않은 어댑터는 "지원하지 않음"(`Client`, HTTP 501로 표시)을 돌려준다. 실제 어댑터는 모두 구현하고,
/// 테스트용 가짜 어댑터는 필요한 것만 구현하면 된다.
#[async_trait::async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    async fn create_booking(&self, request: &CreateBooking) -> AdapterReply<ProviderBooking>;

    /// 우리 참조값(예약 ID)으로 업체의 **확정된** 예약을 찾는다. 없으면 `Ok(None)`.
    /// `capabilities().lookup_by_reference`가 `false`인 어댑터는 부르지 않는다.
    async fn find_booking(&self, reference: BookingId) -> AdapterReply<Option<ProviderBooking>>;

    /// 업체 예약을 취소한다. 이미 취소된 예약도 성공으로 본다 (두 번 보내도 결과가 같다).
    /// 업체에 없는 예약이면 `Rejected { BookingNotFound }`.
    async fn cancel_booking(&self, _provider_booking_id: &str) -> AdapterReply<()> {
        unsupported("cancel_booking")
    }

    /// 슬롯 시작 시각이 `[from, to)`인 업체 예약 목록 (취소된 것 포함). 대사가 쓴다.
    /// `capabilities().list_bookings`가 `false`인 어댑터는 부르지 않는다.
    async fn list_bookings(
        &self,
        _from: DateTime<Utc>,
        _to: DateTime<Utc>,
    ) -> AdapterReply<Vec<ListedBooking>> {
        unsupported("list_bookings")
    }

    /// 자원 하나의 날짜별 빈 시간. `date`는 업체의 현지 날짜다 (로컬 mock 업체는 한국 날짜).
    async fn availability(
        &self,
        _resource_id: &str,
        _date: NaiveDate,
    ) -> AdapterReply<Vec<AvailableSlot>> {
        unsupported("availability")
    }
}

/// 어댑터가 구현하지 않은 기능을 불렀다. 업체 문제가 아니라 우리 쪽 구성 문제라서 `Client`(재시도 · 서킷 집계 없음)로 둔다.
fn unsupported<T>(operation: &str) -> AdapterReply<T> {
    Err(ProviderError::Client {
        status: 501,
        body: format!("어댑터가 {operation}을(를) 지원하지 않습니다"),
    })
    .into()
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("알 수 없는 어댑터 종류입니다: {0}")]
    UnknownKind(String),
    #[error("업체 주소(base_url)가 올바르지 않습니다: {0}")]
    InvalidBaseUrl(String),
    #[error("업체 정책 형식이 올바르지 않습니다: {0}")]
    Policy(#[from] serde_json::Error),
    #[error("업체 정책 값이 올바르지 않습니다: {0}")]
    InvalidPolicy(String),
    #[error("HTTP 클라이언트를 만들 수 없습니다: {0}")]
    Http(#[from] reqwest::Error),
}

/// `providers.adapter_kind`에 맞는 어댑터를 만든다
pub fn build_adapter(kind: &str, base_url: &str) -> Result<Arc<dyn ProviderAdapter>, BuildError> {
    match kind {
        "alpha_v1" => Ok(Arc::new(AlphaAdapter::new(base_url, ADAPTER_TIMEOUT)?)),
        "beta_v1" => Ok(Arc::new(BetaAdapter::new(base_url, ADAPTER_TIMEOUT)?)),
        other => Err(BuildError::UnknownKind(other.to_owned())),
    }
}

/// 업체 주소를 읽는다. 끝의 `/`는 떼고, 경로를 이어 붙일 수 있는 주소(`http://…`)인지 확인한다.
pub(crate) fn parse_base_url(base_url: &str) -> Result<reqwest::Url, BuildError> {
    let url = reqwest::Url::parse(base_url.trim_end_matches('/'))
        .map_err(|error| BuildError::InvalidBaseUrl(format!("{base_url}: {error}")))?;
    if url.cannot_be_a_base() {
        return Err(BuildError::InvalidBaseUrl(base_url.to_owned()));
    }
    Ok(url)
}

/// `base` 뒤에 경로 조각을 이어 붙인 주소. 조각은 퍼센트 인코딩되므로(`/`, `?`도 글자로) 업체가 준 ID를 넣어도 안전하다.
pub(crate) fn join_url(base: &reqwest::Url, segments: &[&str]) -> reqwest::Url {
    let mut url = base.clone();
    // `parse_base_url`에서 경로를 붙일 수 있는 주소인지 확인했으므로 Err가 나오지 않는다
    if let Ok(mut path) = url.path_segments_mut() {
        path.pop_if_empty().extend(segments);
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 경로_조각은_인코딩해서_붙인다() {
        let base = parse_base_url("http://127.0.0.1:9090/alpha/v1/").unwrap();
        assert_eq!(
            join_url(&base, &["reservations", "rsv_1"]).as_str(),
            "http://127.0.0.1:9090/alpha/v1/reservations/rsv_1"
        );
        assert_eq!(
            join_url(&base, &["reservations", "a/b?c"]).as_str(),
            "http://127.0.0.1:9090/alpha/v1/reservations/a%2Fb%3Fc"
        );
    }

    #[test]
    fn 경로를_붙일_수_없는_주소는_거절한다() {
        assert!(matches!(
            parse_base_url("not a url"),
            Err(BuildError::InvalidBaseUrl(_))
        ));
        assert!(matches!(
            parse_base_url("mailto:someone@example.com"),
            Err(BuildError::InvalidBaseUrl(_))
        ));
    }
}
