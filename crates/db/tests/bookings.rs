//! 예약 접수 · 상태 전이 저장소 테스트.
//!
//! `#[sqlx::test]`는 테스트마다 빈 데이터베이스를 새로 만들고 마이그레이션을 적용한 뒤 연결 풀(`pool`)을 넘겨준다.
//! 실행하려면 `DATABASE_URL`이 가리키는 PostgreSQL이 떠 있어야 한다 (`docker compose up -d`).

mod common;

use std::time::Duration;

use db::{
    PgPool, TraceRef,
    bookings::{self, AcceptOutcome, NewBooking, Transitioned},
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
        None,
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
        None,
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
        None,
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

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 접수와_전이를_일으킨_trace를_이력과_작업에_남긴다(pool: PgPool) {
    let client = common::create_client(&pool, "app").await;
    let request_trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let context = serde_json::json!({
        "traceparent": format!("00-{request_trace}-00f067aa0ba902b7-01")
    });
    let new = NewBooking {
        trace: TraceRef {
            trace_id: Some(request_trace.into()),
            context: Some(context.clone()),
        },
        ..common::new_booking(client, "key-1", b"hash-1")
    };
    bookings::accept(&pool, &new).await.unwrap();

    // 작업을 가져가면 등록한 쪽의 trace 문맥이 함께 나온다 (워커가 link를 걸 때 쓴다)
    let job = jobs::claim_next(&pool, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.trace_context, Some(context));

    // 워커는 자기 trace에서 상태를 바꾼다
    let job_trace = "0af7651916cd43dd8448eb211c80319c";
    let mut conn = pool.acquire().await.unwrap();
    let confirmed = BookingEvent::ProviderConfirmed {
        provider_booking_id: "rsv_1".into(),
    };
    bookings::transition(
        &mut conn,
        new.id,
        BookingStatus::Pending,
        &confirmed,
        "worker",
        Some(job_trace),
    )
    .await
    .unwrap();

    let traces: Vec<_> = bookings::events(&pool, new.id)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.trace_id)
        .collect();
    assert_eq!(
        traces,
        vec![Some(request_trace.to_owned()), Some(job_trace.to_owned())]
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 결과_불명으로_가장_오래_머문_시간을_알려_준다(pool: PgPool) {
    assert_eq!(
        bookings::oldest_unknown_age(&pool).await.unwrap(),
        Duration::ZERO,
        "결과 불명 예약이 없으면 0"
    );

    let client = common::create_client(&pool, "app").await;
    for (key, minutes_ago) in [("key-1", 3), ("key-2", 12)] {
        let new = common::new_booking(client, key, key.as_bytes());
        bookings::accept(&pool, &new).await.unwrap();
        // 준비: 몇 분 전에 결과 불명이 된 것으로 만든다
        sqlx::query(
            "UPDATE bookings SET status = 'unknown',
                    updated_at = now() - make_interval(mins => $2) WHERE id = $1",
        )
        .bind(new.id.as_uuid())
        .bind(minutes_ago)
        .execute(&pool)
        .await
        .unwrap();
    }

    let age = bookings::oldest_unknown_age(&pool).await.unwrap();
    assert!(
        (Duration::from_secs(12 * 60)..Duration::from_secs(13 * 60)).contains(&age),
        "가장 오래된 것(12분 전) 기준이어야 한다: {age:?}"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 목록은_최근_접수부터_조건과_커서로_나눠_읽는다(pool: PgPool) {
    let client = common::create_client(&pool, "app").await;
    let mut ids = Vec::new();
    for n in 0..5 {
        let new = common::new_booking(client, &format!("key-{n}"), b"hash");
        bookings::accept(&pool, &new).await.unwrap();
        ids.push(new.id);
    }
    // 하나는 실패 상태로 만든다
    let mut conn = pool.acquire().await.unwrap();
    let event = BookingEvent::GaveUp {
        reason: "test".into(),
    };
    bookings::transition(
        &mut conn,
        ids[1],
        BookingStatus::Pending,
        &event,
        "test",
        None,
    )
    .await
    .unwrap();

    let all = bookings::BookingFilter::default();
    let first_page = bookings::list(&pool, &all, 2).await.unwrap();
    let first_ids: Vec<_> = first_page.iter().map(|b| b.id).collect();
    assert_eq!(first_ids, vec![ids[4], ids[3]], "최근에 접수한 것부터");

    let next = bookings::BookingFilter {
        before: Some(ids[3]),
        ..all.clone()
    };
    let second_page: Vec<_> = bookings::list(&pool, &next, 10)
        .await
        .unwrap()
        .iter()
        .map(|b| b.id)
        .collect();
    assert_eq!(
        second_page,
        vec![ids[2], ids[1], ids[0]],
        "커서 다음부터 이어진다"
    );

    let failed = bookings::BookingFilter {
        status: Some(BookingStatus::Failed),
        ..all.clone()
    };
    let failed: Vec<_> = bookings::list(&pool, &failed, 10)
        .await
        .unwrap()
        .iter()
        .map(|b| b.id)
        .collect();
    assert_eq!(failed, vec![ids[1]]);

    let other_provider = bookings::BookingFilter {
        provider: Some(domain::ProviderId::parse("beta").unwrap()),
        ..all.clone()
    };
    assert!(
        bookings::list(&pool, &other_provider, 10)
            .await
            .unwrap()
            .is_empty()
    );

    let future = bookings::BookingFilter {
        created_from: Some(chrono::Utc::now() + chrono::TimeDelta::hours(1)),
        ..all.clone()
    };
    assert!(bookings::list(&pool, &future, 10).await.unwrap().is_empty());
    let until_now = bookings::BookingFilter {
        created_to: Some(chrono::Utc::now() + chrono::TimeDelta::seconds(1)),
        ..all
    };
    assert_eq!(
        bookings::list(&pool, &until_now, 10).await.unwrap().len(),
        5
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 잠그고_읽은_예약은_트랜잭션이_끝날_때까지_다른_곳에서_바꾸지_못한다(
    pool: PgPool,
) {
    let client = common::create_client(&pool, "app").await;
    let new = common::new_booking(client, "key-1", b"hash-1");
    bookings::accept(&pool, &new).await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let locked = bookings::get_for_update(&mut tx, new.id)
        .await
        .unwrap()
        .expect("예약이 있어야 한다");
    assert_eq!(locked.status, BookingStatus::Pending);

    // 다른 연결에서 같은 예약을 바꾸려 하면 잠금이 풀릴 때까지 기다린다
    let other_pool = pool.clone();
    let id = new.id;
    let waiting = tokio::spawn(async move {
        let mut conn = other_pool.acquire().await.unwrap();
        let event = BookingEvent::GaveUp {
            reason: "late".into(),
        };
        bookings::transition(&mut conn, id, BookingStatus::Pending, &event, "test", None)
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!waiting.is_finished(), "잠금이 풀릴 때까지 기다린다");

    let event = BookingEvent::ProviderConfirmed {
        provider_booking_id: "rsv_1".into(),
    };
    bookings::transition(&mut tx, id, BookingStatus::Pending, &event, "test", None)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        waiting.await.unwrap(),
        Transitioned::Conflict,
        "잠금이 풀린 뒤에는 이미 바뀐 상태를 보고 충돌로 끝난다"
    );
}
