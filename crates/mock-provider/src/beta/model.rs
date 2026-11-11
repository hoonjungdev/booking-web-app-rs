//! beta API의 요청 · 응답 형식 (JSON ↔ Rust 타입)
//!
//! - 필드명은 camelCase: `#[serde(rename_all = "camelCase")]`가 `shop_code` ↔ `shopCode`로 바꿔 준다.
//! - 시각은 한국 시간 문자열: 필드는 `DateTime<Utc>`로 두고 `#[serde(with = "time::...")]`로 경계에서만 바꾼다.
//! - 성공 응답에는 `"ok": true`가 앞에 붙는다 (`beta::Success`가 붙인다). 여기 타입은 그 뒤에 오는 필드다.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::time;

/// `POST /beta/api/book` 요청 본문.
///
/// `startAt`은 일부러 문자열로 받는다. 형식이 틀린 시각을 "본문 해석 실패(`BAD_REQUEST`)"가 아니라
/// 업무 에러 `BAD_TIME`으로 돌려주기 위해서다. 해석은 저장소(`Store::create`)가 한다.
/// 모르는 필드는 무시한다 (serde 기본 동작).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookRequest {
    pub shop_code: String,
    /// 한국 시간 `yyyyMMddHHmm`
    pub start_at: String,
    pub people: u32,
    pub guest_name: String,
    pub guest_phone: String,
    /// 호출한 쪽의 참조값 (우리 서비스는 booking id를 넣는다). 없어도 된다.
    /// 멱등키가 아니다: 같은 값으로 두 번 보내면 예약이 두 개 생긴다.
    #[serde(default)]
    pub external_ref: Option<String>,
}

/// JSON에서는 `"BOOKED"`, `"CANCELLED"`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum BookingStatus {
    Booked,
    Cancelled,
}

/// 저장된 예약.
///
/// ```json
/// {"bookingNo":"BK00000001","shopCode":"GN01","startAt":"202611201900","endAt":"202611202000","people":2,
///  "guestName":"홍길동","guestPhone":"010-0000-0000","externalRef":"0193...","status":"BOOKED",
///  "bookedAt":"202611191230","cancelledAt":null}
/// ```
///
/// 시각 필드는 실제 시각(`DateTime<Utc>`, 초 이하 포함)을 저장하고 JSON에서만 분 단위 한국 시간으로 보인다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Booking {
    /// `BK` + 8자리 일련번호 (예: `BK00000001`)
    pub booking_no: String,
    pub shop_code: String,
    #[serde(rename = "startAt", with = "time::minute")]
    pub slot_start: DateTime<Utc>,
    #[serde(rename = "endAt", with = "time::minute")]
    pub slot_end: DateTime<Utc>,
    pub people: u32,
    pub guest_name: String,
    pub guest_phone: String,
    pub external_ref: Option<String>,
    pub status: BookingStatus,
    #[serde(with = "time::minute")]
    pub booked_at: DateTime<Utc>,
    #[serde(with = "time::optional_minute")]
    pub cancelled_at: Option<DateTime<Utc>>,
}

/// 예약 · 취소 성공 응답: `{"ok":true,"bookingNo":"BK00000001","status":"BOOKED"}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookingResult {
    pub booking_no: String,
    pub status: BookingStatus,
}

/// 예약 목록 응답: `{"ok":true,"bookings":[...]}` (만든 순서)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookingList {
    pub bookings: Vec<Booking>,
}

/// `GET /beta/api/slots` 쿼리.
///
/// 값이 빠지거나 틀렸을 때 axum이 대신 400으로 거절하지 않도록 모두 `Option<String>`으로 받고
/// 핸들러에서 직접 검사한다 (이 업체는 그런 경우에도 HTTP 200 + `BAD_REQUEST`로 답한다).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotsQuery {
    pub shop_code: Option<String>,
    /// 한국 날짜 `yyyyMMdd`
    pub day: Option<String>,
}

/// 슬롯 하나: `{"startAt":"202611201000","endAt":"202611201100","capacity":3,"remain":2}`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotInfo {
    #[serde(with = "time::minute")]
    pub start_at: DateTime<Utc>,
    #[serde(with = "time::minute")]
    pub end_at: DateTime<Utc>,
    pub capacity: u32,
    /// 남은 자리 = 정원 − 이 슬롯의 `BOOKED` 예약 수
    pub remain: u32,
}

/// 슬롯 조회 응답: `{"ok":true,"shopCode":"GN01","day":"20261120","slots":[...]}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShopSlots {
    pub shop_code: String,
    #[serde(with = "time::day")]
    pub day: NaiveDate,
    pub slots: Vec<SlotInfo>,
}

/// `GET /beta/api/bookings` 쿼리 (한국 날짜 `yyyyMMdd`, 둘 다 포함)
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookingsQuery {
    pub from_day: Option<String>,
    pub to_day: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn booking() -> Booking {
        Booking {
            booking_no: "BK00000001".into(),
            shop_code: "GN01".into(),
            slot_start: "2026-11-20T10:00:00Z".parse().unwrap(),
            slot_end: "2026-11-20T11:00:00Z".parse().unwrap(),
            people: 2,
            guest_name: "홍길동".into(),
            guest_phone: "010-0000-0000".into(),
            external_ref: None,
            status: BookingStatus::Booked,
            booked_at: "2026-11-19T03:30:45Z".parse().unwrap(),
            cancelled_at: None,
        }
    }

    #[test]
    fn 예약은_camel_case와_한국시간_문자열로_직렬화된다() {
        assert_eq!(
            serde_json::to_value(booking()).unwrap(),
            json!({
                "bookingNo": "BK00000001",
                "shopCode": "GN01",
                "startAt": "202611201900",
                "endAt": "202611202000",
                "people": 2,
                "guestName": "홍길동",
                "guestPhone": "010-0000-0000",
                "externalRef": null,
                "status": "BOOKED",
                "bookedAt": "202611191230",
                "cancelledAt": null
            })
        );
    }

    #[test]
    fn 직렬화한_예약을_다시_읽으면_분_단위까지_같다() {
        let mut original = booking();
        original.status = BookingStatus::Cancelled;
        original.cancelled_at = Some("2026-11-19T04:00:00Z".parse().unwrap());
        original.external_ref = Some("booking-1".into());

        let text = serde_json::to_string(&original).unwrap();
        assert!(text.contains(r#""cancelledAt":"202611191300""#), "{text}");
        let parsed: Booking = serde_json::from_str(&text).unwrap();

        // JSON에는 분까지만 실리므로 초가 있는 `bookedAt`만 달라진다
        assert_eq!(
            parsed.booked_at,
            "2026-11-19T03:30:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        assert_eq!(
            Booking {
                booked_at: original.booked_at,
                ..parsed
            },
            original
        );
    }

    #[test]
    fn 예약_요청에서_external_ref는_빠져도_된다() {
        let request: BookRequest = serde_json::from_value(json!({
            "shopCode": "GN01",
            "startAt": "202611201900",
            "people": 2,
            "guestName": "홍길동",
            "guestPhone": "010-0000-0000",
            "somethingElse": true
        }))
        .unwrap();
        assert_eq!(request.external_ref, None);
    }
}
