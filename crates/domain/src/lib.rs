//! 예약 도메인 모델, 상태 머신, 도메인 에러 (I/O 없음)
//!
//! 이 크레이트는 DB · HTTP · 시계를 모른다. 현재 시각이 필요한 함수는 호출하는 쪽에서 `now`를 넘겨받고,
//! 새 ID도 호출하는 쪽에서 만들어 넘긴다. 그래서 모든 규칙을 DB 없이 단위 테스트할 수 있다.
//!
//! | 모듈 | 내용 |
//! |---|---|
//! | `ids` | `BookingId`, `ClientId`, `ProviderId` 같은 식별자 타입 |
//! | `status` | 예약 상태 머신: 상태, 상태를 바꾸는 사건, 전이 규칙 (docs/02-domain-and-data.md §2) |
//! | `booking` | 예약 요청과 검증, 확정 기한, 저장된 예약 |

mod booking;
mod ids;
mod status;

// `pub use`로 하위 모듈의 타입을 크레이트 최상위로 꺼낸다. 사용하는 쪽은 `domain::BookingId`처럼 짧게 쓴다.
pub use booking::{
    Booking, BookingRequest, CONFIRMATION_WINDOW, Customer, MAX_PARTY_SIZE, ValidationError,
    confirmation_deadline,
};
pub use ids::{BookingId, ClientId, InvalidProviderId, ProviderId};
pub use status::{BookingEvent, BookingStatus, InvalidTransition, RejectReason, UnknownStatus};
