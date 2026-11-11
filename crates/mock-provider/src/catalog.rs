//! 업체가 가진 자원(매장)과 예약 슬롯.
//!
//! 슬롯과 정원은 DB 없이 "자원 ID + 날짜 + 시각"으로 정해지는 고정 규칙으로 만든다.
//! 그래서 서버를 재시작해도 같은 날짜에는 항상 같은 슬롯과 정원이 나온다.
//!
//! 매장 목록은 업체마다 따로 있다: alpha는 [`RESOURCES`], beta는 [`BETA_SHOPS`].
//! 슬롯 규칙([`slots_for`], [`slot_at`])은 두 업체가 함께 쓰고, 매장 ID(코드)가 다르므로 정원도 다르게 나온다.

use chrono::{DateTime, FixedOffset, NaiveDate, TimeDelta, Utc};

/// 예약 대상 자원 (매장)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resource {
    pub id: &'static str,
    pub name: &'static str,
}

/// alpha 업체가 가진 자원 목록.
/// `&'static str`은 프로그램이 끝날 때까지 살아 있는 문자열(리터럴)을 가리키는 참조다.
pub const RESOURCES: &[Resource] = &[
    Resource {
        id: "store-001",
        name: "강남점",
    },
    Resource {
        id: "store-002",
        name: "홍대점",
    },
    Resource {
        id: "store-003",
        name: "판교점",
    },
];

/// beta 업체의 매장 목록. 같은 매장이라도 레거시 시스템이라 `GN01` 같은 매장 코드를 쓴다.
pub const BETA_SHOPS: &[Resource] = &[
    Resource {
        id: "GN01",
        name: "강남점",
    },
    Resource {
        id: "HD02",
        name: "홍대점",
    },
    Resource {
        id: "PG03",
        name: "판교점",
    },
];

/// 첫 슬롯 시작 시각 (한국 시간)
pub const FIRST_SLOT_HOUR: u32 = 10;
/// 마지막 슬롯 시작 시각 (한국 시간). 20:00~21:00이 마지막 슬롯
pub const LAST_SLOT_HOUR: u32 = 20;
/// 슬롯 하나의 길이
pub const SLOT_LENGTH: TimeDelta = TimeDelta::hours(1);
/// 슬롯 하나에 받을 수 있는 최대 예약 수. 실제 정원은 0..=이 값 (0이면 마감된 슬롯)
pub const MAX_CAPACITY: u32 = 4;

/// 하나의 예약 슬롯
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub capacity: u32,
}

/// 한국 표준시 (UTC+9)
pub fn kst() -> FixedOffset {
    FixedOffset::east_opt(9 * 3600).expect("UTC+9는 항상 올바른 오프셋이다")
}

/// ID로 alpha 자원을 찾는다. 없으면 `None` (≈ C#의 null 대신 쓰는 Option 타입)
pub fn find_resource(id: &str) -> Option<&'static Resource> {
    RESOURCES.iter().find(|r| r.id == id)
}

/// 매장 코드로 beta 매장을 찾는다. 없으면 `None`
pub fn find_beta_shop(code: &str) -> Option<&'static Resource> {
    BETA_SHOPS.iter().find(|r| r.id == code)
}

/// 자원의 특정 날짜(한국 날짜) 슬롯 목록
pub fn slots_for(resource_id: &str, date: NaiveDate) -> Vec<Slot> {
    (FIRST_SLOT_HOUR..=LAST_SLOT_HOUR)
        .filter_map(|hour| {
            // and_hms_opt는 잘못된 시각이면 None을 돌려준다. `?`는 None이면 이 클로저를 바로 None으로 끝낸다.
            let local = date.and_hms_opt(hour, 0, 0)?;
            let start = local
                .and_local_timezone(kst())
                .single()?
                .with_timezone(&Utc);
            Some(Slot {
                start,
                end: start + SLOT_LENGTH,
                capacity: capacity(resource_id, date, hour),
            })
        })
        .collect()
}

/// 시작 시각이 정확히 일치하는 슬롯을 찾는다
pub fn slot_at(resource_id: &str, start: DateTime<Utc>) -> Option<Slot> {
    let date = start.with_timezone(&kst()).date_naive();
    slots_for(resource_id, date)
        .into_iter()
        .find(|slot| slot.start == start)
}

fn capacity(resource_id: &str, date: NaiveDate, hour: u32) -> u32 {
    let key = format!("{resource_id}|{date}|{hour}");
    let bucket = fnv1a(key.as_bytes()) % u64::from(MAX_CAPACITY + 1);
    // bucket은 0..=MAX_CAPACITY 범위이므로 u32로 바꿔도 값이 잘리지 않는다.
    u32::try_from(bucket).unwrap_or(0)
}

/// FNV-1a 해시. 표준 라이브러리의 해시는 Rust 버전마다 결과가 달라질 수 있어
/// "항상 같은 결과"가 필요한 곳에는 이렇게 단순한 해시를 직접 쓴다.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 20).unwrap()
    }

    #[test]
    fn 하루에_11개_슬롯이_한국시간_10시부터_생긴다() {
        let slots = slots_for("store-001", date());
        assert_eq!(slots.len(), 11);
        // 한국시간 10:00 = UTC 01:00
        assert_eq!(slots[0].start.to_rfc3339(), "2026-11-20T01:00:00+00:00");
        assert_eq!(slots[10].end.to_rfc3339(), "2026-11-20T12:00:00+00:00");
    }

    #[test]
    fn 같은_입력이면_정원이_항상_같다() {
        assert_eq!(
            slots_for("store-001", date()),
            slots_for("store-001", date())
        );
        assert!(
            slots_for("store-001", date())
                .iter()
                .all(|s| s.capacity <= MAX_CAPACITY)
        );
    }

    #[test]
    fn beta_매장은_코드로_찾고_alpha_자원과_섞이지_않는다() {
        assert_eq!(find_beta_shop("GN01").map(|r| r.name), Some("강남점"));
        assert_eq!(find_beta_shop("store-001"), None);
        assert_eq!(find_resource("GN01"), None);
        // 같은 규칙이라도 매장 코드가 다르면 정원이 따로 정해진다. 슬롯 시각은 같다.
        let beta = slots_for("GN01", date());
        assert_eq!(beta.len(), 11);
        assert_eq!(beta[0].start, slots_for("store-001", date())[0].start);
    }

    #[test]
    fn 시작_시각이_슬롯_경계가_아니면_찾지_못한다() {
        let slot = slots_for("store-001", date())[0];
        assert_eq!(slot_at("store-001", slot.start), Some(slot));
        assert_eq!(
            slot_at("store-001", slot.start + TimeDelta::minutes(30)),
            None
        );
    }
}
