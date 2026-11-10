//! 예약 요청과 검증, 확정 기한, 저장된 예약

use std::fmt;

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::{BookingId, BookingStatus, ClientId, ProviderId};

/// 한 예약의 최대 인원. 업체마다 더 작은 한도가 있으면 업체가 거절한다.
pub const MAX_PARTY_SIZE: u32 = 50;
/// 접수 후 이 시간 안에 확정하지 못하면 포기한다 (docs/03-resilience.md §4)
pub const CONFIRMATION_WINDOW: TimeDelta = TimeDelta::minutes(30);

const MAX_RESOURCE_ID_LEN: usize = 100;
const MAX_NAME_LEN: usize = 100;
const MAX_PHONE_LEN: usize = 30;

/// 고객 정보.
///
/// 개인정보라서 `Debug`를 직접 구현해 값을 가린다. 실수로 `tracing::info!(?customer)`처럼 찍어도 노출되지 않는다.
/// (`#[derive(Debug)]`를 쓰지 않고 `impl fmt::Debug`를 손으로 쓴 이유)
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Customer {
    pub name: String,
    pub phone: String,
}

impl fmt::Debug for Customer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Customer")
            .field("name", &"***")
            .field("phone", &"***")
            .finish()
    }
}

/// 클라이언트가 요청한 예약 내용
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookingRequest {
    pub provider: ProviderId,
    /// 업체 안의 예약 대상 (매장 등). 업체마다 형식이 달라 우리는 내용을 해석하지 않는다.
    pub resource_id: String,
    pub slot_start: DateTime<Utc>,
    pub party_size: u32,
    pub customer: Customer,
}

/// 예약 요청이 규칙에 맞지 않는 이유
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("resource_id는 공백이 아닌 1~{max}자여야 합니다", max = MAX_RESOURCE_ID_LEN)]
    ResourceId,
    #[error("slot_start는 현재 시각 이후여야 합니다")]
    SlotInPast,
    #[error("party_size는 1~{max}명이어야 합니다", max = MAX_PARTY_SIZE)]
    PartySize,
    #[error("customer.{field}는 공백이 아닌 1~{max}자여야 합니다")]
    Customer { field: &'static str, max: usize },
}

impl BookingRequest {
    /// 규칙을 검사한다. `now`는 호출하는 쪽의 현재 시각.
    ///
    /// 반환 타입 `Result<(), ValidationError>`: 성공이면 돌려줄 값이 없으므로 `()`(빈 튜플, ≈ C#의 void).
    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), ValidationError> {
        if !is_present(&self.resource_id, MAX_RESOURCE_ID_LEN) {
            return Err(ValidationError::ResourceId);
        }
        if self.slot_start <= now {
            return Err(ValidationError::SlotInPast);
        }
        if !(1..=MAX_PARTY_SIZE).contains(&self.party_size) {
            return Err(ValidationError::PartySize);
        }
        if !is_present(&self.customer.name, MAX_NAME_LEN) {
            return Err(ValidationError::Customer {
                field: "name",
                max: MAX_NAME_LEN,
            });
        }
        if !is_present(&self.customer.phone, MAX_PHONE_LEN) {
            return Err(ValidationError::Customer {
                field: "phone",
                max: MAX_PHONE_LEN,
            });
        }
        Ok(())
    }
}

/// 공백만 있지 않고 `max`자(문자 수 기준) 이하인가
fn is_present(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.chars().count() <= max
}

/// 확정 기한: 접수 후 [`CONFIRMATION_WINDOW`]가 지나거나 슬롯이 시작하는 시각 중 이른 쪽
pub fn confirmation_deadline(now: DateTime<Utc>, slot_start: DateTime<Utc>) -> DateTime<Utc> {
    (now + CONFIRMATION_WINDOW).min(slot_start)
}

/// 저장된 예약
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Booking {
    pub id: BookingId,
    pub client_id: ClientId,
    pub request: BookingRequest,
    pub status: BookingStatus,
    /// 마지막 상태 전이의 사유 (거절 코드, 포기 사유 등)
    pub status_reason: Option<String>,
    /// 업체가 확정한 뒤 알려 준 업체 쪽 예약 ID
    pub provider_booking_id: Option<String>,
    pub deadline_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-11-10T00:00:00Z".parse().unwrap()
    }

    fn request() -> BookingRequest {
        BookingRequest {
            provider: ProviderId::parse("alpha").unwrap(),
            resource_id: "store-001".into(),
            slot_start: now() + TimeDelta::days(1),
            party_size: 2,
            customer: Customer {
                name: "홍길동".into(),
                phone: "010-0000-0000".into(),
            },
        }
    }

    #[test]
    fn 올바른_요청은_통과한다() {
        assert_eq!(request().validate(now()), Ok(()));
    }

    #[test]
    fn 규칙에_맞지_않는_요청은_거절한다() {
        let cases: Vec<(BookingRequest, ValidationError)> = vec![
            (
                BookingRequest {
                    resource_id: "  ".into(),
                    ..request()
                },
                ValidationError::ResourceId,
            ),
            (
                BookingRequest {
                    slot_start: now(),
                    ..request()
                },
                ValidationError::SlotInPast,
            ),
            (
                BookingRequest {
                    party_size: 0,
                    ..request()
                },
                ValidationError::PartySize,
            ),
            (
                BookingRequest {
                    party_size: MAX_PARTY_SIZE + 1,
                    ..request()
                },
                ValidationError::PartySize,
            ),
            (
                BookingRequest {
                    customer: Customer {
                        name: String::new(),
                        phone: "010".into(),
                    },
                    ..request()
                },
                ValidationError::Customer {
                    field: "name",
                    max: MAX_NAME_LEN,
                },
            ),
        ];
        for (request, error) in cases {
            assert_eq!(request.validate(now()), Err(error));
        }
    }

    #[test]
    fn 글자_수는_바이트가_아니라_문자_수로_센다() {
        // 한글 한 글자는 UTF-8로 3바이트지만 한 글자로 센다.
        let mut request = request();
        request.customer.name = "가".repeat(MAX_NAME_LEN);
        assert_eq!(request.validate(now()), Ok(()));
    }

    #[test]
    fn 확정_기한은_30분과_슬롯_시작_중_이른_쪽이다() {
        let far_slot = now() + TimeDelta::days(1);
        assert_eq!(
            confirmation_deadline(now(), far_slot),
            now() + TimeDelta::minutes(30)
        );

        let near_slot = now() + TimeDelta::minutes(10);
        assert_eq!(confirmation_deadline(now(), near_slot), near_slot);
    }

    #[test]
    fn 고객_정보는_debug_출력에서_가려진다() {
        let printed = format!("{:?}", request());
        assert!(!printed.contains("홍길동"));
        assert!(!printed.contains("010-0000-0000"));
        assert!(printed.contains("***"));
    }
}
