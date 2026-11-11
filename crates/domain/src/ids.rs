//! 식별자 타입
//!
//! `struct BookingId(Uuid)`처럼 값 하나를 감싼 타입을 "newtype"이라 부른다.
//! 속은 똑같이 `Uuid`여도 타입이 다르므로, 예약 ID 자리에 클라이언트 ID를 넣는 실수를 컴파일러가 막아 준다.
//! (≈ C#의 `readonly record struct BookingId(Guid Value)`)

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 예약 ID. 업체에 보내는 참조값과 멱등키로도 그대로 쓴다.
///
/// `#[serde(transparent)]`: JSON에서는 감싼 값(UUID 문자열) 그대로 보이게 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
// `cfg_attr(조건, 속성)`: 조건이 참일 때만 속성을 붙인다. `openapi` feature를 켠 빌드에서만 API 문서용 스키마를 만든다.
// 문서 설명은 따로 적는다 (없으면 위의 학습용 주석이 그대로 API 문서에 실린다).
#[cfg_attr(
    feature = "openapi",
    derive(utoipa::ToSchema),
    schema(description = "예약 ID (UUID v7)")
)]
pub struct BookingId(Uuid);

impl BookingId {
    pub fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

/// `Display`를 구현하면 `format!("{id}")`, `id.to_string()`을 쓸 수 있다 (≈ C#의 `ToString()` 재정의).
impl fmt::Display for BookingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// 우리 예약 API를 호출하는 내부 클라이언트의 ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(
    feature = "openapi",
    derive(utoipa::ToSchema),
    schema(description = "API 클라이언트 ID (UUID v7)")
)]
pub struct ClientId(Uuid);

impl ClientId {
    pub fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// 외부 업체 ID (`alpha`, `beta` …).
///
/// 영문 소문자로 시작하고 소문자 · 숫자 · `_` · `-`로 이루어진 1~32자.
/// 만들 때 검사하므로 `ProviderId` 값이 있다면 항상 올바른 형식이다 ("검증된 타입" 패턴).
/// JSON에서 읽을 때도 `try_from = "String"` 덕분에 같은 검사를 거친다.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[cfg_attr(
    feature = "openapi",
    derive(utoipa::ToSchema),
    schema(
        description = "업체 ID (영문 소문자로 시작하는 1~32자의 소문자 · 숫자 · `_` · `-`)",
        example = "alpha"
    )
)]
pub struct ProviderId(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("업체 ID는 영문 소문자로 시작하는 1~32자의 소문자 · 숫자 · '_' · '-'여야 합니다: {0:?}")]
pub struct InvalidProviderId(pub String);

impl ProviderId {
    pub const MAX_LEN: usize = 32;

    pub fn parse(value: &str) -> Result<Self, InvalidProviderId> {
        let mut chars = value.chars();
        let valid_first = chars.next().is_some_and(|c| c.is_ascii_lowercase());
        let valid_rest =
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if valid_first && valid_rest && value.len() <= Self::MAX_LEN {
            Ok(Self(value.to_owned()))
        } else {
            Err(InvalidProviderId(value.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProviderId {
    type Error = InvalidProviderId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<ProviderId> for String {
    fn from(id: ProviderId) -> Self {
        id.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 올바른_업체_id만_만들어진다() {
        assert!(ProviderId::parse("alpha").is_ok());
        assert!(ProviderId::parse("beta_2").is_ok());
        assert!(ProviderId::parse("").is_err());
        assert!(ProviderId::parse("Alpha").is_err());
        assert!(ProviderId::parse("1alpha").is_err());
        assert!(ProviderId::parse("al pha").is_err());
        assert!(ProviderId::parse(&"a".repeat(33)).is_err());
    }

    #[test]
    fn json에서_읽을_때도_검사한다() {
        let ok: Result<ProviderId, _> = serde_json::from_str(r#""alpha""#);
        let bad: Result<ProviderId, _> = serde_json::from_str(r#""ALPHA""#);
        assert_eq!(ok.unwrap().as_str(), "alpha");
        assert!(bad.is_err());
    }
}
