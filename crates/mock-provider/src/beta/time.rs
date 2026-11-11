//! beta의 시각 형식: 오프셋이 없는 **한국 시간** 문자열.
//!
//! | 형식 | 예 | 뜻 |
//! |---|---|---|
//! | `yyyyMMddHHmm` | `202611201900` | 2026-11-20 19:00 한국 시간 (= 10:00 UTC) |
//! | `yyyyMMdd` | `20261120` | 한국 날짜 |
//!
//! 저장소 안에서는 항상 `DateTime<Utc>`로 다루고, JSON으로 주고받는 경계에서만 이 문자열로 바꾼다.
//! 구조체 필드에 `#[serde(with = "time::minute")]`처럼 붙이면 직렬화할 때 아래 모듈의 함수를 쓴다
//! (≈ C# System.Text.Json에서 속성에 `[JsonConverter(typeof(...))]`를 붙이는 것).

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

use crate::catalog::kst;

/// chrono의 형식 문자열. `%Y%m%d%H%M` = `yyyyMMddHHmm` (≈ C#의 `DateTime.ParseExact(text, "yyyyMMddHHmm", ...)`)
const MINUTE_FORMAT: &str = "%Y%m%d%H%M";
const DAY_FORMAT: &str = "%Y%m%d";

/// UTC 시각 → 한국 시간 `yyyyMMddHHmm` (초 이하는 버린다)
pub fn format_minute(at: DateTime<Utc>) -> String {
    at.with_timezone(&kst()).format(MINUTE_FORMAT).to_string()
}

/// 한국 시간 `yyyyMMddHHmm` → UTC 시각. 형식이 틀리면 `None`
pub fn parse_minute(text: &str) -> Option<DateTime<Utc>> {
    // chrono는 숫자 앞의 공백을 건너뛰고 자릿수가 모자라도 받아 준다 ("2026 11 20 19 00" 등).
    // 레거시 형식은 정확히 숫자 12자리이므로 먼저 직접 확인한다.
    if !is_digits(text, 12) {
        return None;
    }
    let local = NaiveDateTime::parse_from_str(text, MINUTE_FORMAT).ok()?;
    // `?`는 Option에도 쓸 수 있다: None이면 바로 None을 돌려준다.
    // 고정 오프셋(UTC+9)이라 한국 시간 하나는 항상 UTC 시각 하나에 대응한다 (`single()`이 None이 되지 않는다).
    let at = local.and_local_timezone(kst()).single()?;
    Some(at.with_timezone(&Utc))
}

/// 날짜 → `yyyyMMdd`
pub fn format_day(day: NaiveDate) -> String {
    day.format(DAY_FORMAT).to_string()
}

/// `yyyyMMdd` → 날짜. 형식이 틀리거나 없는 날짜(11월 31일 등)면 `None`
pub fn parse_day(text: &str) -> Option<NaiveDate> {
    if !is_digits(text, 8) {
        return None;
    }
    NaiveDate::parse_from_str(text, DAY_FORMAT).ok()
}

/// UTC 시각이 한국 날짜로 며칠인지
pub fn kst_day(at: DateTime<Utc>) -> NaiveDate {
    at.with_timezone(&kst()).date_naive()
}

fn is_digits(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| b.is_ascii_digit())
}

/// `#[serde(with = "time::minute")]`: `DateTime<Utc>` ↔ `"yyyyMMddHHmm"`
///
/// serde가 정해 둔 모양대로 `serialize` · `deserialize` 두 함수를 둔다.
/// `'de`는 "읽고 있는 원본 데이터가 살아 있는 동안"을 뜻하는 lifetime 이름이다. serde 규칙을 그대로 따르면 된다.
pub mod minute {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(at: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::format_minute(*at))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DateTime<Utc>, D::Error> {
        let text = String::deserialize(deserializer)?;
        super::parse_minute(&text).ok_or_else(|| Error::custom("yyyyMMddHHmm 형식이 아닙니다"))
    }
}

/// `#[serde(with = "time::optional_minute")]`: `Option<DateTime<Utc>>` ↔ `"yyyyMMddHHmm"` 또는 `null`
pub mod optional_minute {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(
        at: &Option<DateTime<Utc>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match at {
            Some(at) => serializer.serialize_str(&super::format_minute(*at)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<DateTime<Utc>>, D::Error> {
        match Option::<String>::deserialize(deserializer)? {
            Some(text) => super::parse_minute(&text)
                .map(Some)
                .ok_or_else(|| Error::custom("yyyyMMddHHmm 형식이 아닙니다")),
            None => Ok(None),
        }
    }
}

/// `#[serde(with = "time::day")]`: `NaiveDate` ↔ `"yyyyMMdd"`
pub mod day {
    use chrono::NaiveDate;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(day: &NaiveDate, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::format_day(*day))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<NaiveDate, D::Error> {
        let text = String::deserialize(deserializer)?;
        super::parse_day(&text).ok_or_else(|| Error::custom("yyyyMMdd 형식이 아닙니다"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    #[test]
    fn 한국시간_문자열을_utc로_바꾼다() {
        // 한국 시간 10:00 = UTC 01:00
        assert_eq!(
            parse_minute("202611201000"),
            Some(utc("2026-11-20T01:00:00Z"))
        );
        assert_eq!(
            parse_minute("202611201900"),
            Some(utc("2026-11-20T10:00:00Z"))
        );
    }

    #[test]
    fn utc를_한국시간_문자열로_바꾼다() {
        assert_eq!(format_minute(utc("2026-11-20T01:00:00Z")), "202611201000");
        // UTC로는 20일이어도 한국 시간으로는 21일이다
        assert_eq!(format_minute(utc("2026-11-20T15:30:59Z")), "202611210030");
        assert_eq!(
            kst_day(utc("2026-11-20T15:30:00Z")).to_string(),
            "2026-11-21"
        );
    }

    #[test]
    fn 형식이_조금이라도_다르면_받지_않는다() {
        for text in [
            "",
            "2026112010",               // 자릿수 부족
            "20261120100",              // 11자리 (chrono만 쓰면 분이 0으로 해석된다)
            "2026112010000",            // 13자리
            "2026-11-20T10:00",         // 다른 형식
            "2026 11 20 10 00",         // 공백 (chrono만 쓰면 해석된다)
            " 202611201000",            // 앞 공백
            "20261120100a",             // 숫자가 아님
            "202613201000",             // 13월
            "202611201060",             // 60분
            "202611311000",             // 11월 31일
            "２０２６１１２０１０００", // 전각 숫자
        ] {
            assert_eq!(parse_minute(text), None, "{text:?}");
        }
    }

    #[test]
    fn 날짜_문자열을_바꾼다() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        assert_eq!(parse_day("20261120"), Some(day));
        assert_eq!(format_day(day), "20261120");
        for text in ["2026112", "202611200", "2026-11-20", "20261131", "2026112a"] {
            assert_eq!(parse_day(text), None, "{text:?}");
        }
    }
}
