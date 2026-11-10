//! 예약 접수 · 상태 전이 저장소 테스트.
//!
//! `#[sqlx::test]`는 테스트마다 빈 데이터베이스를 새로 만들고 마이그레이션을 적용한 뒤 연결 풀(`pool`)을 넘겨준다.
//! 실행하려면 `DATABASE_URL`이 가리키는 PostgreSQL이 떠 있어야 한다 (`docker compose up -d`).

mod common;

use db::{
    PgPool,
    bookings::{self, AcceptOutcome, Transitioned},
    jobs,
};
use domain::{BookingEvent, BookingStatus, RejectReason};

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 접수하면_예약_이력_작업이_함께_저장된다(pool: PgPool) {
    let client = common::create_client(&pool, "app").await;
    let new = common::new_booking(client, "key-1", b"hash-1");

    let AcceptOutcome::Created(booking) = bookings::accept(&pool, &new).await.unwrap() else {
        panic!("새로 만들어져야 한다");
    };

    assert_eq!(booking.id, new.id);
    assert_eq!(booking.status, BookingStatus::Pending);
    assert_eq!(booking.request, new.request);

    let events = bookings::events(&pool, booking.id).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].from_status, None);
    assert_eq!(events[0].to_status, BookingStatus::Pending);
    assert_eq!(events[0].actor, "api");

    let jobs = jobs::for_booking(&pool, booking.id).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, "submit_booking");
    assert_eq!(jobs[0].status, "queued");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 같은_멱등키로_다시_접수하면_기존_예약을_돌려준다(pool: PgPool) {
    let client = common::create_client(&pool, "app").await;
    let first = common::new_booking(client, "key-1", b"hash-1");
    let AcceptOutcome::Created(created) = bookings::accept(&pool, &first).await.unwrap() else {
        panic!("처음은 새로 만들어져야 한다");
    };

    // 같은 키 + 같은 해시: 새 ID로 다시 보내도 처음 예약이 돌아온다
    let again = common::new_booking(client, "key-1", b"hash-1");
    let outcome = bookings::accept(&pool, &again).await.unwrap();
    assert_eq!(
        outcome,
        AcceptOutcome::Existing {
            booking: created.clone(),
            same_request: true,
        }
    );

    // 같은 키 + 다른 해시: 기존 예약이지만 다른 요청으로 표시된다
    let different = common::new_booking(client, "key-1", b"hash-2");
    let AcceptOutcome::Existing { same_request, .. } =
        bookings::accept(&pool, &different).await.unwrap()
    else {
        panic!("기존 예약이어야 한다");
    };
    assert!(!same_request);

    // 작업은 처음 한 번만 등록된다
    assert_eq!(jobs::for_booking(&pool, created.id).await.unwrap().len(), 1);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 멱등키는_클라이언트별로_따로_센다(pool: PgPool) {
    let a = common::create_client(&pool, "app-a").await;
    let b = common::create_client(&pool, "app-b").await;

    let outcome_a = bookings::accept(&pool, &common::new_booking(a, "same-key", b"h"))
        .await
        .unwrap();
    let outcome_b = bookings::accept(&pool, &common::new_booking(b, "same-key", b"h"))
        .await
        .unwrap();

    assert!(matches!(outcome_a, AcceptOutcome::Created(_)));
    assert!(matches!(outcome_b, AcceptOutcome::Created(_)));
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 상태_전이는_도메인_규칙을_따르고_이력을_남긴다(pool: PgPool) {
    let client = common::create_client(&pool, "app").await;
    let new = common::new_booking(client, "key-1", b"hash-1");
    bookings::accept(&pool, &new).await.unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let confirmed = BookingEvent::ProviderConfirmed {
        provider_booking_id: "rsv_1".into(),
    };
    let result = bookings::transition(
        &mut conn,
        new.id,
        BookingStatus::Pending,
        &confirmed,
        "worker",
    )
    .await
    .unwrap();
    assert_eq!(result, Transitioned::Applied(BookingStatus::Confirmed));

    let booking = bookings::get(&pool, new.id).await.unwrap().unwrap();
    assert_eq!(booking.status, BookingStatus::Confirmed);
    assert_eq!(booking.provider_booking_id.as_deref(), Some("rsv_1"));

    // 이미 confirmed인데 pending에서 바꾸려 하면 충돌 (아무것도 바뀌지 않음)
    let rejected = BookingEvent::ProviderRejected {
        reason: RejectReason::SlotUnavailable,
    };
    let result = bookings::transition(
        &mut conn,
        new.id,
        BookingStatus::Pending,
        &rejected,
        "worker",
    )
    .await
    .unwrap();
    assert_eq!(result, Transitioned::Conflict);

    // 규칙에 없는 전이는 저장 전에 거절된다 (confirmed → rejected)
    let result = bookings::transition(
        &mut conn,
        new.id,
        BookingStatus::Confirmed,
        &rejected,
        "worker",
    )
    .await;
    assert!(matches!(result, Err(db::DbError::InvalidTransition(_))));

    let events = bookings::events(&pool, new.id).await.unwrap();
    let path: Vec<_> = events.iter().map(|e| e.to_status).collect();
    assert_eq!(path, vec![BookingStatus::Pending, BookingStatus::Confirmed]);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 다른_클라이언트의_예약은_보이지_않는다(pool: PgPool) {
    let owner = common::create_client(&pool, "owner").await;
    let other = common::create_client(&pool, "other").await;
    let new = common::new_booking(owner, "key-1", b"hash-1");
    bookings::accept(&pool, &new).await.unwrap();

    assert!(
        bookings::find_for_client(&pool, owner, new.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        bookings::find_for_client(&pool, other, new.id)
            .await
            .unwrap()
            .is_none()
    );
}
