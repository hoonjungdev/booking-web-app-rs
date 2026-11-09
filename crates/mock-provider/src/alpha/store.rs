//! alpha 업체의 예약 저장소와 업무 규칙 (메모리 저장).
//!
//! HTTP와 분리해 두었기 때문에 서버를 띄우지 않고도 규칙을 단위 테스트할 수 있다.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use uuid::Uuid;

use super::{
    error::AlphaError,
    model::{
        CreateReservationRequest, Reservation, ReservationQuery, ReservationStatus,
        SlotAvailability,
    },
};
use crate::catalog;

const MAX_PARTY_SIZE: u32 = 8;
const MAX_REFERENCE_LEN: usize = 100;

/// 예약 생성 결과
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateOutcome {
    /// 새로 만듦 (201)
    Created(Reservation),
    /// 같은 멱등키 + 같은 본문이라 이전 결과를 그대로 돌려줌 (200)
    Replayed(Reservation),
}

/// `Default`를 derive하면 `Store::default()`로 빈 저장소를 만들 수 있다.
#[derive(Debug, Default)]
pub struct Store {
    /// 예약 ID → 예약
    reservations: HashMap<String, Reservation>,
    /// 멱등키 → (처음 받은 요청, 만든 예약 ID)
    idempotency: HashMap<String, (CreateReservationRequest, String)>,
}

impl Store {
    /// 예약을 만든다.
    ///
    /// 반환 타입 `Result<성공, 실패>`: 실패할 수 있는 함수는 예외를 던지지 않고 이렇게 결과로 알린다.
    pub fn create(
        &mut self,
        request: CreateReservationRequest,
        idempotency_key: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<CreateOutcome, AlphaError> {
        // 1) 이미 본 멱등키면: 본문이 같을 때만 이전 결과를 돌려준다.
        if let Some(key) = idempotency_key
            && let Some((previous, id)) = self.idempotency.get(key)
        {
            if *previous != request {
                return Err(AlphaError::IdempotencyKeyMismatch);
            }
            // `?`: Err면 그 자리에서 함수를 끝내고 Err를 그대로 돌려준다.
            let reservation = self.get(id)?;
            return Ok(CreateOutcome::Replayed(reservation));
        }

        // 2) 입력 검증
        validate(&request)?;
        let resource = catalog::find_resource(&request.resource_id)
            .ok_or_else(|| AlphaError::ResourceNotFound(request.resource_id.clone()))?;
        let slot =
            catalog::slot_at(resource.id, request.slot_start).ok_or(AlphaError::InvalidSlot)?;
        if slot.start <= now {
            return Err(AlphaError::SlotInPast);
        }

        // 3) 정원 확인
        if self.booked_count(resource.id, slot.start) >= slot.capacity {
            return Err(AlphaError::SlotUnavailable);
        }

        // 4) 저장
        let reservation = Reservation {
            id: format!("rsv_{}", Uuid::now_v7().simple()),
            resource_id: resource.id.to_owned(),
            slot_start: slot.start,
            slot_end: slot.end,
            party_size: request.party_size,
            customer: request.customer.clone(),
            client_reference: request.client_reference.clone(),
            status: ReservationStatus::Confirmed,
            created_at: now,
            cancelled_at: None,
        };
        self.reservations
            .insert(reservation.id.clone(), reservation.clone());
        if let Some(key) = idempotency_key {
            // request는 여기서 map으로 "이동(move)"한다. 이후에는 request를 쓸 수 없다.
            self.idempotency
                .insert(key.to_owned(), (request, reservation.id.clone()));
        }
        Ok(CreateOutcome::Created(reservation))
    }

    pub fn get(&self, id: &str) -> Result<Reservation, AlphaError> {
        self.reservations
            .get(id)
            .cloned()
            .ok_or_else(|| AlphaError::ReservationNotFound(id.to_owned()))
    }

    /// 예약을 취소한다. 이미 취소된 예약이면 그대로 돌려준다 (멱등).
    pub fn cancel(&mut self, id: &str, now: DateTime<Utc>) -> Result<Reservation, AlphaError> {
        let reservation = self
            .reservations
            .get_mut(id)
            .ok_or_else(|| AlphaError::ReservationNotFound(id.to_owned()))?;
        if reservation.status == ReservationStatus::Confirmed {
            reservation.status = ReservationStatus::Cancelled;
            reservation.cancelled_at = Some(now);
        }
        Ok(reservation.clone())
    }

    /// 조건에 맞는 예약 목록 (생성 순서대로)
    pub fn search(&self, query: &ReservationQuery) -> Vec<Reservation> {
        let mut found: Vec<Reservation> = self
            .reservations
            .values()
            .filter(|r| {
                query
                    .client_reference
                    .as_ref()
                    .is_none_or(|reference| r.client_reference.as_ref() == Some(reference))
            })
            .filter(|r| query.from.is_none_or(|from| r.slot_start >= from))
            .filter(|r| query.to.is_none_or(|to| r.slot_start < to))
            .cloned()
            .collect();
        found.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        found
    }

    /// 자원의 날짜별 남은 자리
    pub fn availability(
        &self,
        resource_id: &str,
        date: NaiveDate,
    ) -> Result<Vec<SlotAvailability>, AlphaError> {
        let resource = catalog::find_resource(resource_id)
            .ok_or_else(|| AlphaError::ResourceNotFound(resource_id.to_owned()))?;
        Ok(catalog::slots_for(resource.id, date)
            .into_iter()
            .map(|slot| SlotAvailability {
                start: slot.start,
                end: slot.end,
                capacity: slot.capacity,
                remaining: slot
                    .capacity
                    .saturating_sub(self.booked_count(resource.id, slot.start)),
            })
            .collect())
    }

    fn booked_count(&self, resource_id: &str, slot_start: DateTime<Utc>) -> u32 {
        let count = self
            .reservations
            .values()
            .filter(|r| {
                r.status == ReservationStatus::Confirmed
                    && r.resource_id == resource_id
                    && r.slot_start == slot_start
            })
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }
}

fn validate(request: &CreateReservationRequest) -> Result<(), AlphaError> {
    if !(1..=MAX_PARTY_SIZE).contains(&request.party_size) {
        return Err(AlphaError::InvalidRequest(format!(
            "party_size는 1~{MAX_PARTY_SIZE}명이어야 합니다"
        )));
    }
    if request.customer.name.trim().is_empty() || request.customer.phone.trim().is_empty() {
        return Err(AlphaError::InvalidRequest(
            "customer.name과 customer.phone은 비어 있을 수 없습니다".into(),
        ));
    }
    if request
        .client_reference
        .as_ref()
        .is_some_and(|r| r.is_empty() || r.len() > MAX_REFERENCE_LEN)
    {
        return Err(AlphaError::InvalidRequest(format!(
            "client_reference는 1~{MAX_REFERENCE_LEN}자여야 합니다"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::alpha::model::Customer;

    fn now() -> DateTime<Utc> {
        "2026-11-09T00:00:00Z".parse().unwrap()
    }

    /// 정원이 1 이상인 첫 슬롯
    fn open_slot() -> catalog::Slot {
        let date = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        catalog::slots_for("store-001", date)
            .into_iter()
            .find(|s| s.capacity > 0)
            .unwrap()
    }

    fn request(slot_start: DateTime<Utc>) -> CreateReservationRequest {
        CreateReservationRequest {
            resource_id: "store-001".into(),
            slot_start,
            party_size: 2,
            customer: Customer {
                name: "홍길동".into(),
                phone: "010-0000-0000".into(),
            },
            client_reference: Some("booking-1".into()),
        }
    }

    #[test]
    fn 정원을_다_채우면_더_받지_않는다() {
        let mut store = Store::default();
        let slot = open_slot();
        for _ in 0..slot.capacity {
            store.create(request(slot.start), None, now()).unwrap();
        }
        assert_eq!(
            store.create(request(slot.start), None, now()),
            Err(AlphaError::SlotUnavailable)
        );
    }

    #[test]
    fn 취소하면_자리가_다시_생긴다() {
        let mut store = Store::default();
        let slot = open_slot();
        let CreateOutcome::Created(created) =
            store.create(request(slot.start), None, now()).unwrap()
        else {
            panic!("새로 만들어져야 한다");
        };
        store.cancel(&created.id, now()).unwrap();

        let date = slot.start.with_timezone(&catalog::kst()).date_naive();
        let availability = store.availability("store-001", date).unwrap();
        let same = availability.iter().find(|s| s.start == slot.start).unwrap();
        assert_eq!(same.remaining, slot.capacity);
    }

    #[test]
    fn 같은_멱등키와_같은_본문이면_이전_결과를_돌려준다() {
        let mut store = Store::default();
        let slot = open_slot();
        let first = store
            .create(request(slot.start), Some("key-1"), now())
            .unwrap();
        let second = store
            .create(request(slot.start), Some("key-1"), now())
            .unwrap();

        let (CreateOutcome::Created(a), CreateOutcome::Replayed(b)) = (first, second) else {
            panic!("처음은 Created, 두 번째는 Replayed여야 한다");
        };
        assert_eq!(a, b);
        assert_eq!(store.search(&ReservationQuery::default()).len(), 1);
    }

    #[test]
    fn 같은_멱등키에_다른_본문이면_거절한다() {
        let mut store = Store::default();
        let slot = open_slot();
        store
            .create(request(slot.start), Some("key-1"), now())
            .unwrap();

        let mut different = request(slot.start);
        different.party_size = 3;
        assert_eq!(
            store.create(different, Some("key-1"), now()),
            Err(AlphaError::IdempotencyKeyMismatch)
        );
    }

    #[test]
    fn 지난_슬롯이나_경계가_아닌_시각은_거절한다() {
        let mut store = Store::default();
        let slot = open_slot();
        let after_slot = slot.end + TimeDelta::days(1);
        assert_eq!(
            store.create(request(slot.start), None, after_slot),
            Err(AlphaError::SlotInPast)
        );
        assert_eq!(
            store.create(request(slot.start + TimeDelta::minutes(1)), None, now()),
            Err(AlphaError::InvalidSlot)
        );
    }
}
