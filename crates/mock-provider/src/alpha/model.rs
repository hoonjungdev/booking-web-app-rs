//! alpha API의 요청 · 응답 형식 (JSON ↔ Rust 타입)

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Customer {
    pub name: String,
    pub phone: String,
}

/// `POST /alpha/v1/reservations` 요청 본문.
/// 같은 멱등키로 다시 왔을 때 본문이 같은지 비교하려고 `PartialEq`를 붙였다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReservationRequest {
    pub resource_id: String,
    /// RFC 3339. `+09:00` 같은 오프셋이 붙어 와도 UTC로 바뀌어 저장된다.
    pub slot_start: DateTime<Utc>,
    pub party_size: u32,
    pub customer: Customer,
    /// 호출한 쪽의 참조값 (우리 서비스는 booking id를 넣는다). 없어도 된다.
    #[serde(default)]
    pub client_reference: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationStatus {
    Confirmed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub id: String,
    pub resource_id: String,
    pub slot_start: DateTime<Utc>,
    pub slot_end: DateTime<Utc>,
    pub party_size: u32,
    pub customer: Customer,
    pub client_reference: Option<String>,
    pub status: ReservationStatus,
    pub created_at: DateTime<Utc>,
    pub cancelled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservationList {
    pub reservations: Vec<Reservation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceInfo {
    pub id: String,
    pub name: String,
    pub timezone: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceList {
    pub resources: Vec<ResourceInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AvailabilityQuery {
    pub resource_id: String,
    /// 한국 날짜 `YYYY-MM-DD`
    pub date: NaiveDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotAvailability {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub capacity: u32,
    pub remaining: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Availability {
    pub resource_id: String,
    pub date: NaiveDate,
    pub slots: Vec<SlotAvailability>,
}

/// `GET /alpha/v1/reservations` 검색 조건. 모두 생략하면 전체 목록.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReservationQuery {
    pub client_reference: Option<String>,
    /// 슬롯 시작 시각이 이 시각 이후 (포함)
    pub from: Option<DateTime<Utc>>,
    /// 슬롯 시작 시각이 이 시각 이전 (미포함)
    pub to: Option<DateTime<Utc>>,
}
