//! db 통합 테스트가 함께 쓰는 준비 코드.
//! `tests/` 아래 파일은 각각 별도 테스트 바이너리라서, 공용 코드는 `tests/common/mod.rs`에 두고 `mod common;`으로 불러온다.

#![allow(dead_code)] // 테스트 파일마다 쓰는 헬퍼가 달라서, 일부 파일에서는 안 쓰는 함수가 생긴다

use chrono::{TimeDelta, Utc};
use db::{PgPool, TraceRef, bookings::NewBooking, clients};
use domain::{BookingId, BookingRequest, ClientId, Customer, ProviderId, confirmation_deadline};
use uuid::Uuid;

pub async fn create_client(pool: &PgPool, name: &str) -> ClientId {
    let id = ClientId::from_uuid(Uuid::now_v7());
    clients::create(pool, id, name, &format!("hash-of-{name}"))
        .await
        .expect("클라이언트를 만들 수 있어야 한다");
    id
}

pub fn request() -> BookingRequest {
    BookingRequest {
        provider: ProviderId::parse("alpha").expect("올바른 업체 ID"),
        resource_id: "store-001".into(),
        // 저장 후 다시 읽은 값과 비교하므로 DB 정밀도(마이크로초)로 맞춘다 (Linux의 Utc::now()는 나노초까지 있다)
        slot_start: db::normalize_timestamp(Utc::now() + TimeDelta::days(1)),
        party_size: 2,
        customer: Customer {
            name: "홍길동".into(),
            phone: "010-0000-0000".into(),
        },
    }
}

pub fn new_booking(client_id: ClientId, idempotency_key: &str, request_hash: &[u8]) -> NewBooking {
    let request = request();
    NewBooking {
        id: BookingId::from_uuid(Uuid::now_v7()),
        client_id,
        idempotency_key: idempotency_key.into(),
        request_hash: request_hash.to_vec(),
        deadline_at: confirmation_deadline(Utc::now(), request.slot_start),
        request,
        trace: TraceRef::default(),
    }
}
