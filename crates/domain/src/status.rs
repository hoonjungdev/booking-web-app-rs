//! 예약 상태 머신 (docs/02-domain-and-data.md §2)
//!
//! 예약 상태는 [`BookingStatus::apply`]를 거쳐서만 바뀐다.
//! "현재 상태 + 일어난 사건 → 다음 상태"를 한 함수에 모아 두어 규칙이 흩어지지 않게 한다.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// 예약 상태.
///
/// Rust의 `enum`은 C#의 enum처럼 이름 붙은 값 목록이다. 아래 [`BookingEvent`]처럼 값마다 데이터를 담을 수도 있다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "openapi",
    derive(utoipa::ToSchema),
    schema(description = "예약 상태 (docs/02-domain-and-data.md §2)")
)]
pub enum BookingStatus {
    /// 접수됨. 업체 제출을 기다리거나 진행 중
    Pending,
    /// 업체가 확정함
    Confirmed,
    /// 업체가 업무적으로 거절함 (종료 상태)
    Rejected,
    /// 생성 요청을 보냈지만 성공 여부를 알 수 없음
    Unknown,
    /// 기술적 실패로 포기함. 관리자가 재시도할 수 있음
    Failed,
    /// 취소 요청 접수. 업체 취소 진행 중
    CancelRequested,
    /// 취소 완료 (종료 상태)
    Cancelled,
    /// 취소 재시도 소진. 관리자 개입 필요
    CancelFailed,
}

impl BookingStatus {
    pub const ALL: [Self; 8] = [
        Self::Pending,
        Self::Confirmed,
        Self::Rejected,
        Self::Unknown,
        Self::Failed,
        Self::CancelRequested,
        Self::Cancelled,
        Self::CancelFailed,
    ];

    /// DB와 API에서 쓰는 문자열
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Confirmed => "confirmed",
            Self::Rejected => "rejected",
            Self::Unknown => "unknown",
            Self::Failed => "failed",
            Self::CancelRequested => "cancel_requested",
            Self::Cancelled => "cancelled",
            Self::CancelFailed => "cancel_failed",
        }
    }

    /// 더 이상 바뀌지 않는 상태인가
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Rejected | Self::Cancelled)
    }

    /// 사건 하나를 적용한 다음 상태. 허용되지 않는 조합이면 `Err`.
    ///
    /// `match (상태, 사건)`은 두 값을 한 번에 패턴으로 비교한다 (≈ C#의 튜플 switch 식).
    /// `{ .. }`는 "사건에 담긴 값은 여기서 보지 않는다"는 뜻이다.
    pub fn apply(self, event: &BookingEvent) -> Result<Self, InvalidTransition> {
        use BookingEvent as E;

        let next = match (self, event) {
            (Self::Pending, E::ProviderConfirmed { .. }) => Self::Confirmed,
            (Self::Pending, E::ProviderRejected { .. }) => Self::Rejected,
            (Self::Pending, E::OutcomeUnknown { .. }) => Self::Unknown,
            (Self::Pending | Self::Unknown, E::GaveUp { .. }) => Self::Failed,
            // 실패한 예약도 대사(reconciliation)가 업체에서 확정된 것을 찾으면 확정으로 바로잡는다 (Phase 6)
            (Self::Unknown | Self::Failed, E::FoundAtProvider { .. }) => Self::Confirmed,
            (Self::Unknown, E::NotFoundAtProvider) => Self::Pending,
            (Self::Failed, E::AdminRetry) => Self::Pending,
            (Self::Confirmed, E::CancelRequested) => Self::CancelRequested,
            (Self::Confirmed, E::CancelledExternally) => Self::Cancelled,
            (Self::CancelRequested, E::CancelConfirmed) => Self::Cancelled,
            (Self::CancelRequested, E::CancelGaveUp { .. }) => Self::CancelFailed,
            (Self::CancelFailed, E::AdminRetry) => Self::CancelRequested,
            (from, event) => {
                return Err(InvalidTransition {
                    from,
                    event: event.name(),
                });
            }
        };
        Ok(next)
    }
}

impl fmt::Display for BookingStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("알 수 없는 예약 상태입니다: {0:?}")]
pub struct UnknownStatus(pub String);

/// DB에서 읽은 문자열을 상태로 바꾼다. `"pending".parse::<BookingStatus>()`처럼 쓴다.
impl FromStr for BookingStatus {
    type Err = UnknownStatus;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
            .ok_or_else(|| UnknownStatus(value.to_owned()))
    }
}

/// 업체가 업무적으로 거절한 이유
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// 슬롯 정원이 찼음
    SlotUnavailable,
    /// 업체에 없는 자원(매장 등)
    ResourceNotFound,
    /// 업체에 그 예약이 없음 (취소하려는 예약을 업체가 모른다)
    BookingNotFound,
    /// 업체가 요청 내용을 받아들이지 않음. `code`는 업체가 준 에러 코드
    InvalidRequest { code: String },
}

impl RejectReason {
    /// `bookings.status_reason`에 남기는 코드
    pub fn code(&self) -> String {
        match self {
            Self::SlotUnavailable => "slot_unavailable".to_owned(),
            Self::ResourceNotFound => "resource_not_found".to_owned(),
            Self::BookingNotFound => "booking_not_found".to_owned(),
            Self::InvalidRequest { code } => format!("invalid_request:{code}"),
        }
    }
}

/// 예약 상태를 바꾸는 사건. 사건마다 필요한 데이터를 함께 담는다.
///
/// (≈ C#에서 `abstract record BookingEvent`와 하위 record들로 만드는 판별 공용체)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookingEvent {
    /// 업체가 예약을 확정함
    ProviderConfirmed { provider_booking_id: String },
    /// 업체가 업무적으로 거절함
    ProviderRejected { reason: RejectReason },
    /// 생성 요청의 결과를 알 수 없음 (타임아웃 등)
    OutcomeUnknown { detail: String },
    /// 기술적 실패로 포기함 (재시도 소진, 기한 초과 등)
    GaveUp { reason: String },
    /// 결과 불명이거나 실패로 끝난 예약을 업체에서 찾음 (결과 불명 확인 작업, 대사)
    FoundAtProvider { provider_booking_id: String },
    /// 결과 불명이던 예약이 업체에 없음 → 다시 제출
    NotFoundAtProvider,
    /// 관리자가 재시도를 지시함
    AdminRetry,
    /// 클라이언트가 취소를 요청함
    CancelRequested,
    /// 업체가 취소를 완료함
    CancelConfirmed,
    /// 취소 재시도 소진
    CancelGaveUp { reason: String },
    /// 대사 작업이 업체 쪽 취소를 발견함
    CancelledExternally,
}

impl BookingEvent {
    /// 로그와 에러 메시지에 쓰는 사건 이름
    pub fn name(&self) -> &'static str {
        match self {
            Self::ProviderConfirmed { .. } => "provider_confirmed",
            Self::ProviderRejected { .. } => "provider_rejected",
            Self::OutcomeUnknown { .. } => "outcome_unknown",
            Self::GaveUp { .. } => "gave_up",
            Self::FoundAtProvider { .. } => "found_at_provider",
            Self::NotFoundAtProvider => "not_found_at_provider",
            Self::AdminRetry => "admin_retry",
            Self::CancelRequested => "cancel_requested",
            Self::CancelConfirmed => "cancel_confirmed",
            Self::CancelGaveUp { .. } => "cancel_gave_up",
            Self::CancelledExternally => "cancelled_externally",
        }
    }

    /// `bookings.status_reason`과 `booking_events.reason`에 남길 설명. 없으면 `None`.
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::ProviderRejected { reason } => Some(reason.code()),
            Self::OutcomeUnknown { detail } => Some(detail.clone()),
            Self::GaveUp { reason } | Self::CancelGaveUp { reason } => Some(reason.clone()),
            Self::ProviderConfirmed { .. }
            | Self::FoundAtProvider { .. }
            | Self::NotFoundAtProvider
            | Self::AdminRetry
            | Self::CancelRequested
            | Self::CancelConfirmed
            | Self::CancelledExternally => None,
        }
    }

    /// 이 사건이 알려 주는 업체 쪽 예약 ID
    pub fn provider_booking_id(&self) -> Option<&str> {
        match self {
            Self::ProviderConfirmed {
                provider_booking_id,
            }
            | Self::FoundAtProvider {
                provider_booking_id,
            } => Some(provider_booking_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{from} 상태에서는 {event} 사건을 처리할 수 없습니다")]
pub struct InvalidTransition {
    pub from: BookingStatus,
    pub event: &'static str,
}

#[cfg(test)]
mod tests {
    use super::{BookingEvent as E, BookingStatus as S, *};

    /// 테스트에 쓰는 모든 사건 (사건마다 하나씩)
    fn all_events() -> Vec<E> {
        vec![
            E::ProviderConfirmed {
                provider_booking_id: "rsv_1".into(),
            },
            E::ProviderRejected {
                reason: RejectReason::SlotUnavailable,
            },
            E::OutcomeUnknown {
                detail: "timeout".into(),
            },
            E::GaveUp {
                reason: "attempts_exhausted".into(),
            },
            E::FoundAtProvider {
                provider_booking_id: "rsv_1".into(),
            },
            E::NotFoundAtProvider,
            E::AdminRetry,
            E::CancelRequested,
            E::CancelConfirmed,
            E::CancelGaveUp {
                reason: "attempts_exhausted".into(),
            },
            E::CancelledExternally,
        ]
    }

    /// 상태나 사건을 추가했는데 `ALL` · `all_events()`에 넣지 않으면 이 함수가 컴파일되지 않는다.
    /// (`_` 없이 모든 경우를 나열한 match는 경우가 하나라도 빠지면 컴파일 에러가 난다)
    #[allow(dead_code)] // 컴파일 검사가 목적이라 호출하지 않는다
    fn 목록이_빠짐없는지_컴파일러가_검사한다(status: S, event: &E) {
        match status {
            S::Pending
            | S::Confirmed
            | S::Rejected
            | S::Unknown
            | S::Failed
            | S::CancelRequested
            | S::Cancelled
            | S::CancelFailed => {}
        }
        match event {
            E::ProviderConfirmed { .. }
            | E::ProviderRejected { .. }
            | E::OutcomeUnknown { .. }
            | E::GaveUp { .. }
            | E::FoundAtProvider { .. }
            | E::NotFoundAtProvider
            | E::AdminRetry
            | E::CancelRequested
            | E::CancelConfirmed
            | E::CancelGaveUp { .. }
            | E::CancelledExternally => {}
        }
    }

    /// 설계 문서의 전이 표. 여기 없는 (상태, 사건) 조합은 모두 거절되어야 한다.
    fn expected(status: S, event: &E) -> Option<S> {
        let table: &[(S, &str, S)] = &[
            (S::Pending, "provider_confirmed", S::Confirmed),
            (S::Pending, "provider_rejected", S::Rejected),
            (S::Pending, "outcome_unknown", S::Unknown),
            (S::Pending, "gave_up", S::Failed),
            (S::Unknown, "found_at_provider", S::Confirmed),
            (S::Unknown, "not_found_at_provider", S::Pending),
            (S::Unknown, "gave_up", S::Failed),
            (S::Failed, "admin_retry", S::Pending),
            (S::Failed, "found_at_provider", S::Confirmed),
            (S::Confirmed, "cancel_requested", S::CancelRequested),
            (S::Confirmed, "cancelled_externally", S::Cancelled),
            (S::CancelRequested, "cancel_confirmed", S::Cancelled),
            (S::CancelRequested, "cancel_gave_up", S::CancelFailed),
            (S::CancelFailed, "admin_retry", S::CancelRequested),
        ];
        table
            .iter()
            .find(|(from, name, _)| *from == status && *name == event.name())
            .map(|(_, _, to)| *to)
    }

    #[test]
    fn 모든_상태와_사건_조합이_전이표와_일치한다() {
        let mut allowed = 0;
        for status in S::ALL {
            for event in all_events() {
                let result = status.apply(&event);
                match expected(status, &event) {
                    Some(to) => {
                        assert_eq!(result, Ok(to), "{status} + {}", event.name());
                        allowed += 1;
                    }
                    None => assert!(
                        result.is_err(),
                        "{status} + {} 는 거절되어야 한다",
                        event.name()
                    ),
                }
            }
        }
        assert_eq!(allowed, 14, "전이표의 14개 전이가 모두 검사되어야 한다");
    }

    #[test]
    fn 종료_상태에서는_어떤_사건도_받지_않는다() {
        for status in S::ALL.into_iter().filter(|s| s.is_terminal()) {
            for event in all_events() {
                assert!(status.apply(&event).is_err());
            }
        }
    }

    #[test]
    fn 상태는_문자열로_왕복_변환된다() {
        for status in S::ALL {
            assert_eq!(status.as_str().parse::<S>(), Ok(status));
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", status.as_str()));
        }
        assert!("PENDING".parse::<S>().is_err());
    }

    #[test]
    fn 사건이_남기는_사유와_업체_예약_id() {
        let rejected = E::ProviderRejected {
            reason: RejectReason::InvalidRequest {
                code: "invalid_slot".into(),
            },
        };
        assert_eq!(
            rejected.reason().as_deref(),
            Some("invalid_request:invalid_slot")
        );

        let confirmed = E::ProviderConfirmed {
            provider_booking_id: "rsv_1".into(),
        };
        assert_eq!(confirmed.provider_booking_id(), Some("rsv_1"));
        assert_eq!(confirmed.reason(), None);
    }
}
