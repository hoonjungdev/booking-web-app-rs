//! 작업 큐 테스트: 가져가기, 재예약, 포기, lease 만료 후 회수, fencing

mod common;

use std::time::Duration;

use db::{PgPool, bookings, jobs};

/// 예약 하나를 접수해 `submit_booking` 작업 하나를 만든다
async fn one_job(pool: &PgPool) -> domain::BookingId {
    let client = common::create_client(pool, "app").await;
    let new = common::new_booking(client, "key-1", b"hash-1");
    bookings::accept(pool, &new)
        .await
        .expect("예약을 접수할 수 있어야 한다");
    new.id
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 작업은_한_번에_하나의_워커만_가져간다(pool: PgPool) {
    let booking_id = one_job(&pool).await;

    let job = jobs::claim_next(&pool, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.booking_id, Some(booking_id));
    assert_eq!(job.attempts, 1);
    assert_eq!(job.max_attempts, jobs::DEFAULT_MAX_ATTEMPTS);

    // lease가 남아 있는 동안에는 아무도 다시 가져가지 못한다
    assert!(
        jobs::claim_next(&pool, Duration::from_secs(60))
            .await
            .unwrap()
            .is_none()
    );

    let mut conn = pool.acquire().await.unwrap();
    assert!(jobs::complete(&mut conn, job.lease()).await.unwrap());
    let record = &jobs::for_booking(&pool, booking_id).await.unwrap()[0];
    assert_eq!(record.status, "done");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 재예약한_작업은_시간이_되기_전에는_나오지_않는다(pool: PgPool) {
    let booking_id = one_job(&pool).await;
    let job = jobs::claim_next(&pool, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let delay = Duration::from_secs(300);
    assert!(
        jobs::retry_later(&mut conn, job.lease(), delay, "timeout")
            .await
            .unwrap()
    );

    assert!(
        jobs::claim_next(&pool, Duration::from_secs(60))
            .await
            .unwrap()
            .is_none()
    );
    let record = &jobs::for_booking(&pool, booking_id).await.unwrap()[0];
    assert_eq!(record.status, "queued");
    assert_eq!(record.attempts, 1);
    assert_eq!(record.last_error.as_deref(), Some("timeout"));
    assert!(record.run_at > chrono::Utc::now() + chrono::TimeDelta::seconds(200));
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn lease가_만료되면_다른_워커가_회수하고_원래_워커의_처리는_무시된다(
    pool: PgPool,
) {
    one_job(&pool).await;

    // 워커 A가 가져갔지만 lease(0초)가 바로 만료된 상황 (처리 도중 죽었다고 가정)
    let first = jobs::claim_next(&pool, Duration::ZERO)
        .await
        .unwrap()
        .unwrap();
    // 워커 B가 같은 작업을 회수한다. attempts가 늘어난다.
    let second = jobs::claim_next(&pool, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(second.attempts, first.attempts + 1);

    // 늦게 깨어난 워커 A의 완료 처리는 반영되지 않는다 (fencing)
    let mut conn = pool.acquire().await.unwrap();
    assert!(!jobs::complete(&mut conn, first.lease()).await.unwrap());
    assert!(
        !jobs::mark_dead(&mut conn, first.lease(), "late")
            .await
            .unwrap()
    );
    // 워커 B의 처리는 반영된다
    assert!(jobs::complete(&mut conn, second.lease()).await.unwrap());
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 포기한_작업은_dead로_남는다(pool: PgPool) {
    let booking_id = one_job(&pool).await;
    let job = jobs::claim_next(&pool, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();

    let mut conn = pool.acquire().await.unwrap();
    assert!(
        jobs::mark_dead(&mut conn, job.lease(), "attempts_exhausted")
            .await
            .unwrap()
    );

    let record = &jobs::for_booking(&pool, booking_id).await.unwrap()[0];
    assert_eq!(record.status, "dead");
    assert_eq!(record.last_error.as_deref(), Some("attempts_exhausted"));
    assert!(
        jobs::claim_next(&pool, Duration::from_secs(60))
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 미룬_작업은_시도_횟수를_되돌린다(pool: PgPool) {
    let booking_id = one_job(&pool).await;
    let job = jobs::claim_next(&pool, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.attempts, 1);

    let mut conn = pool.acquire().await.unwrap();
    assert!(
        jobs::defer(&mut conn, job.lease(), Duration::ZERO, "circuit_open")
            .await
            .unwrap()
    );
    let record = &jobs::for_booking(&pool, booking_id).await.unwrap()[0];
    assert_eq!(record.status, "queued");
    assert_eq!(
        record.attempts, 0,
        "업체를 부르지 않았으므로 시도로 세지 않는다"
    );
    assert_eq!(record.last_error.as_deref(), Some("circuit_open"));

    // 다시 가져가면 다시 1번째 시도다.
    // (미룬 워커는 defer가 마지막 쓰기라서, 같은 시도 번호가 다시 나와도 그 워커가 이 작업을 건드릴 일은 없다)
    let again = jobs::claim_next(&pool, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.attempts, 1);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 진행_상황을_payload에_남기고_다시_가져갈_때_읽는다(pool: PgPool) {
    one_job(&pool).await;
    let job = jobs::claim_next(&pool, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.payload, serde_json::json!({}));

    let mut conn = pool.acquire().await.unwrap();
    let progress = serde_json::json!({ "not_found_checks": 1 });
    assert!(
        jobs::retry_later_with_payload(
            &mut conn,
            job.lease(),
            Duration::ZERO,
            "not_found",
            &progress
        )
        .await
        .unwrap()
    );

    let again = jobs::claim_next(&pool, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.payload, progress);
    assert_eq!(again.attempts, 2);
}
