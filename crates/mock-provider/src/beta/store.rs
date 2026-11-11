//! beta 업체의 예약 저장소와 업무 규칙 (메모리 저장).
//!
//! HTTP와 분리해 두었기 때문에 서버를 띄우지 않고도 규칙을 단위 테스트할 수 있다.
//! alpha와 가장 큰 차이는 **멱등키가 없다**는 것이다. 같은 `externalRef`로 두 번 보내면 예약이 두 개 생긴다.

use chrono::{DateTime, NaiveDate, Utc};

use super::{
    error::BetaError,
    model::{BookRequest, Booking, BookingStatus, SlotInfo},
    time,
};
use crate::catalog;

const MAX_PEOPLE: u32 = 8;
const MAX_REFERENCE_CHARS: usize = 100;

/// `Default`를 derive하면 `Store::default()`로 빈 저장소를 만들 수 있다.
#[derive(Debug, Default)]
pub struct Store {
    /// 만든 순서대로 쌓는다. 취소해도 지우지 않으므로 "목록 길이 + 1"이 다음 예약 번호다.
    bookings: Vec<Booking>,
}

impl Store {
    /// 예약을 만든다.
    ///
    /// 멱등키가 없으므로 같은 요청이 다시 오면 **새 예약을 또 만든다** (일부러 이렇게 둔다).
    /// 우리 게이트웨이는 결과가 불분명할 때 다시 보내지 말고 `externalRef`로 조회해야 한다.
    pub fn create(
        &mut self,
        request: BookRequest,
        now: DateTime<Utc>,
    ) -> Result<Booking, BetaError> {
        // 1) 입력 검증
        validate(&request)?;
        let shop = catalog::find_beta_shop(&request.shop_code)
            .ok_or_else(|| BetaError::NoShop(request.shop_code.clone()))?;

        // 2) 한국 시간 문자열 → UTC. 슬롯 시작 시각과 정확히 같아야 한다.
        let start = time::parse_minute(&request.start_at).ok_or(BetaError::BadTime)?;
        let slot = catalog::slot_at(shop.id, start).ok_or(BetaError::BadTime)?;
        if slot.start <= now {
            return Err(BetaError::Past);
        }

        // 3) 정원 확인 (취소된 예약은 세지 않는다)
        if self.booked_count(shop.id, slot.start) >= slot.capacity {
            return Err(BetaError::Full);
        }

        // 4) 저장. 이름 · 전화번호 · 참조값은 request에서 "이동(move)"해 온다 (복사하지 않음).
        let booking = Booking {
            booking_no: format!("BK{:08}", self.bookings.len() + 1),
            shop_code: shop.id.to_owned(),
            slot_start: slot.start,
            slot_end: slot.end,
            people: request.people,
            guest_name: request.guest_name,
            guest_phone: request.guest_phone,
            external_ref: request.external_ref,
            status: BookingStatus::Booked,
            booked_at: now,
            cancelled_at: None,
        };
        self.bookings.push(booking.clone());
        Ok(booking)
    }

    pub fn get(&self, booking_no: &str) -> Result<Booking, BetaError> {
        self.bookings
            .iter()
            .find(|b| b.booking_no == booking_no)
            .cloned()
            .ok_or_else(|| BetaError::NotFound(booking_no.to_owned()))
    }

    /// 예약을 취소한다. alpha와 달리 멱등이 아니다: 이미 취소된 예약이면 `ALREADY_CANCELLED`.
    pub fn cancel(&mut self, booking_no: &str, now: DateTime<Utc>) -> Result<Booking, BetaError> {
        // iter_mut: 목록 안의 값을 바꿀 수 있는 참조로 하나씩 꺼낸다
        let booking = self
            .bookings
            .iter_mut()
            .find(|b| b.booking_no == booking_no)
            .ok_or_else(|| BetaError::NotFound(booking_no.to_owned()))?;
        if booking.status == BookingStatus::Cancelled {
            return Err(BetaError::AlreadyCancelled);
        }
        booking.status = BookingStatus::Cancelled;
        booking.cancelled_at = Some(now);
        Ok(booking.clone())
    }

    /// 참조값이 같은 예약 전부 (상태 무관, 만든 순서). 없으면 빈 목록.
    pub fn by_ref(&self, external_ref: &str) -> Vec<Booking> {
        self.bookings
            .iter()
            .filter(|b| b.external_ref.as_deref() == Some(external_ref))
            .cloned()
            .collect()
    }

    /// 슬롯 시작 시각의 한국 날짜가 `from_day` ~ `to_day`(둘 다 포함)인 예약 (상태 무관, 만든 순서)
    pub fn by_days(&self, from_day: NaiveDate, to_day: NaiveDate) -> Vec<Booking> {
        // `a..=b`는 끝을 포함하는 범위다 (≈ C#의 `a <= x && x <= b`)
        let days = from_day..=to_day;
        self.bookings
            .iter()
            .filter(|b| days.contains(&time::kst_day(b.slot_start)))
            .cloned()
            .collect()
    }

    /// 저장된 예약 전부 (만든 순서)
    pub fn all(&self) -> Vec<Booking> {
        self.bookings.clone()
    }

    /// 매장의 날짜별(한국 날짜) 슬롯과 남은 자리
    pub fn slots(&self, shop_code: &str, day: NaiveDate) -> Result<Vec<SlotInfo>, BetaError> {
        let shop = catalog::find_beta_shop(shop_code)
            .ok_or_else(|| BetaError::NoShop(shop_code.to_owned()))?;
        Ok(catalog::slots_for(shop.id, day)
            .into_iter()
            .map(|slot| SlotInfo {
                start_at: slot.start,
                end_at: slot.end,
                capacity: slot.capacity,
                remain: slot
                    .capacity
                    .saturating_sub(self.booked_count(shop.id, slot.start)),
            })
            .collect())
    }

    fn booked_count(&self, shop_code: &str, slot_start: DateTime<Utc>) -> u32 {
        let count = self
            .bookings
            .iter()
            .filter(|b| {
                b.status == BookingStatus::Booked
                    && b.shop_code == shop_code
                    && b.slot_start == slot_start
            })
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }
}

fn validate(request: &BookRequest) -> Result<(), BetaError> {
    if !(1..=MAX_PEOPLE).contains(&request.people) {
        return Err(BetaError::BadRequest(format!(
            "people는 1~{MAX_PEOPLE}명이어야 합니다"
        )));
    }
    if request.guest_name.trim().is_empty() || request.guest_phone.trim().is_empty() {
        return Err(BetaError::BadRequest(
            "guestName과 guestPhone은 비어 있을 수 없습니다".into(),
        ));
    }
    // 바이트 수(`len()`)가 아니라 글자 수로 센다 (한글 한 글자는 UTF-8로 3바이트)
    if request
        .external_ref
        .as_ref()
        .is_some_and(|r| r.is_empty() || r.chars().count() > MAX_REFERENCE_CHARS)
    {
        return Err(BetaError::BadRequest(format!(
            "externalRef는 1~{MAX_REFERENCE_CHARS}자여야 합니다"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Timelike};

    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-11-09T00:00:00Z".parse().unwrap()
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 20).unwrap()
    }

    /// 정원이 2 이상인 첫 슬롯 (같은 슬롯에 예약을 여러 개 만들어 볼 수 있게)
    fn open_slot() -> catalog::Slot {
        catalog::slots_for("GN01", day())
            .into_iter()
            .find(|s| s.capacity >= 2)
            .unwrap()
    }

    fn request(slot_start: DateTime<Utc>, external_ref: &str) -> BookRequest {
        BookRequest {
            shop_code: "GN01".into(),
            start_at: time::format_minute(slot_start),
            people: 2,
            guest_name: "홍길동".into(),
            guest_phone: "010-0000-0000".into(),
            external_ref: Some(external_ref.into()),
        }
    }

    #[test]
    fn 한국시간_문자열의_슬롯으로_예약하고_번호를_매긴다() {
        let mut store = Store::default();
        let slot = open_slot();
        // 슬롯 시각을 한국 시간 글자로 직접 만든다 (예: 19시 → "202611201900" = 10:00 UTC).
        // 이날 슬롯(한국 10~20시)은 UTC로도 같은 날짜(01~11시)라서 "UTC 시 + 9"가 한국 시다.
        let kst_hour = slot.start.hour() + 9;
        let mut first = request(slot.start, "booking-1");
        first.start_at = format!("20261120{kst_hour:02}00");

        let a = store.create(first, now()).unwrap();
        let b = store
            .create(request(slot.start, "booking-2"), now())
            .unwrap();
        assert_eq!(a.booking_no, "BK00000001");
        assert_eq!(b.booking_no, "BK00000002");
        assert_eq!(a.slot_start, slot.start);
        assert_eq!(a.slot_end, slot.end);
        assert_eq!(a.status, BookingStatus::Booked);
        assert_eq!(a.booked_at, now());
        assert_eq!(store.get("BK00000002"), Ok(b));
    }

    #[test]
    fn 멱등키가_없어서_같은_참조값도_예약이_두_개_생긴다() {
        let mut store = Store::default();
        let slot = open_slot();
        let first = store
            .create(request(slot.start, "booking-1"), now())
            .unwrap();
        let second = store
            .create(request(slot.start, "booking-1"), now())
            .unwrap();

        assert_ne!(first.booking_no, second.booking_no);
        assert_eq!(store.by_ref("booking-1"), vec![first, second]);
        assert!(store.by_ref("booking-2").is_empty());
    }

    #[test]
    fn 정원을_다_채우면_full() {
        let mut store = Store::default();
        let slot = open_slot();
        for i in 0..slot.capacity {
            store
                .create(request(slot.start, &format!("booking-{i}")), now())
                .unwrap();
        }
        assert_eq!(
            store.create(request(slot.start, "one-more"), now()),
            Err(BetaError::Full)
        );
    }

    #[test]
    fn 두_번_취소하면_already_cancelled() {
        let mut store = Store::default();
        let slot = open_slot();
        let created = store
            .create(request(slot.start, "booking-1"), now())
            .unwrap();
        let later = now() + TimeDelta::minutes(5);

        let cancelled = store.cancel(&created.booking_no, later).unwrap();
        assert_eq!(cancelled.status, BookingStatus::Cancelled);
        assert_eq!(cancelled.cancelled_at, Some(later));
        assert_eq!(
            store.cancel(&created.booking_no, later),
            Err(BetaError::AlreadyCancelled)
        );
        assert_eq!(
            store.cancel("BK99999999", later),
            Err(BetaError::NotFound("BK99999999".into()))
        );
        // 취소한 예약도 참조값 조회에는 나온다
        assert_eq!(store.by_ref("booking-1"), vec![cancelled]);
    }

    #[test]
    fn 취소하면_자리가_다시_생긴다() {
        let mut store = Store::default();
        let slot = open_slot();
        let created = store
            .create(request(slot.start, "booking-1"), now())
            .unwrap();
        let remain = |store: &Store| {
            store
                .slots("GN01", day())
                .unwrap()
                .into_iter()
                .find(|s| s.start_at == slot.start)
                .unwrap()
                .remain
        };
        assert_eq!(remain(&store), slot.capacity - 1);

        store.cancel(&created.booking_no, now()).unwrap();
        assert_eq!(remain(&store), slot.capacity);
    }

    #[test]
    fn 기간_조회는_한국_날짜_양끝을_포함한다() {
        let mut store = Store::default();
        let days: Vec<NaiveDate> = (0..3).map(|n| day() + TimeDelta::days(n)).collect();
        let mut made = Vec::new();
        for (i, day) in days.iter().enumerate() {
            let slot = catalog::slots_for("GN01", *day)
                .into_iter()
                .find(|s| s.capacity > 0)
                .unwrap();
            made.push(
                store
                    .create(request(slot.start, &format!("booking-{i}")), now())
                    .unwrap(),
            );
        }

        assert_eq!(store.by_days(days[0], days[1]), made[..2].to_vec());
        assert_eq!(store.by_days(days[1], days[2]), made[1..].to_vec());
        assert_eq!(store.by_days(days[2], days[2]), made[2..].to_vec());
        assert!(
            store
                .by_days(days[2] + TimeDelta::days(1), days[2] + TimeDelta::days(9))
                .is_empty()
        );
        assert_eq!(store.all(), made);
    }

    #[test]
    fn 지난_슬롯이나_경계가_아닌_시각은_거절한다() {
        let mut store = Store::default();
        let slot = open_slot();
        let after_slot = slot.end + TimeDelta::days(1);
        assert_eq!(
            store.create(request(slot.start, "x"), after_slot),
            Err(BetaError::Past)
        );
        // 슬롯 시작 시각과 같은 순간도 지난 것으로 본다
        assert_eq!(
            store.create(request(slot.start, "x"), slot.start),
            Err(BetaError::Past)
        );

        for start_at in ["202611201030", "202611202100", "2026-11-20T10:00", ""] {
            let mut bad = request(slot.start, "x");
            bad.start_at = start_at.into();
            assert_eq!(
                store.create(bad, now()),
                Err(BetaError::BadTime),
                "{start_at:?}"
            );
        }
        assert!(store.all().is_empty());
    }

    #[test]
    fn 잘못된_입력은_bad_request이고_없는_매장은_no_shop() {
        let mut store = Store::default();
        let slot = open_slot();
        let cases: [fn(&mut BookRequest); 6] = [
            |r| r.people = 0,
            |r| r.people = 9,
            |r| r.guest_name = " ".into(),
            |r| r.guest_phone = String::new(),
            |r| r.external_ref = Some(String::new()),
            |r| r.external_ref = Some("가".repeat(101)),
        ];
        for change in cases {
            let mut bad = request(slot.start, "x");
            change(&mut bad);
            assert!(
                matches!(store.create(bad, now()), Err(BetaError::BadRequest(_))),
                "BAD_REQUEST여야 한다"
            );
        }

        // 100자는 받는다 (바이트로는 300바이트)
        let mut long = request(slot.start, "x");
        long.external_ref = Some("가".repeat(100));
        long.people = 8;
        assert!(store.create(long, now()).is_ok());

        let mut unknown = request(slot.start, "x");
        unknown.shop_code = "store-001".into();
        assert_eq!(
            store.create(unknown, now()),
            Err(BetaError::NoShop("store-001".into()))
        );
        assert_eq!(
            store.slots("XX99", day()),
            Err(BetaError::NoShop("XX99".into()))
        );
    }
}
