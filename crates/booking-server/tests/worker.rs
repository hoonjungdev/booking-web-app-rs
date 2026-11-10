//! 워커 테스트: 가짜 어댑터로 업체 응답을 정해 두고, `process_next`로 한 단계씩 처리해 결과를 확인한다.

mod common;

use std::{sync::Arc, time::Duration};

use axum::http::Method;
use booking_server::{
    registry::ProviderRegistry,
    worker::{self, WorkerConfig, WorkerContext},
};
use db::PgPool;
use domain::{BookingId, BookingStatus, ProviderId, RejectReason};
use providers::{ProviderBooking, ProviderError};

use common::{FakeAdapter, IDEMPOTENT, NOT_IDEMPOTENT, booking_body, create_client, send};

/// 예약 하나를 API로 접수하고 ID를 돌려준다
async fn accept_booking(pool: &PgPool) -> BookingId {
    let client = create_client(pool, "app").await;
    let slot = (chrono::Utc::now() + chrono::TimeDelta::days(1)).to_rfc3339();
    send(
        &common::app(pool),
        Method::POST,
        "/v1/bookings",
        &[
            ("x-api-key", &client.api_key),
            ("idempotency-key", "order-1"),
        ],
        Some(&booking_body(&slot)),
    )
    .await
    .booking()
    .id
}

/// 가짜 어댑터를 alpha 자리에 넣은 워커
fn worker_with(pool: &PgPool, adapter: &Arc<FakeAdapter>) -> WorkerContext {
    let mut registry = ProviderRegistry::default();
    let alpha = ProviderId::parse("alpha").expect("올바른 업체 ID");
    // Arc<FakeAdapter>는 Arc<dyn ProviderAdapter>로 자동 변환된다
    registry.insert(alpha, adapter.clone());
    WorkerContext {
        pool: pool.clone(),
        providers: Arc::new(registry),
        config: WorkerConfig {
            concurrency: 1,
            poll_interval: Duration::from_millis(10),
            lease: Duration::from_secs(60),
        },
    }
}

fn confirmed(id: &str) -> Result<ProviderBooking, ProviderError> {
    Ok(ProviderBooking {
        provider_booking_id: id.into(),
    })
}

async fn booking(pool: &PgPool, id: BookingId) -> domain::Booking {
    db::bookings::get(pool, id)
        .await
        .expect("예약을 읽을 수 있어야 한다")
        .expect("예약이 있어야 한다")
}

async fn job(pool: &PgPool, id: BookingId) -> db::jobs::JobRecord {
    db::jobs::for_booking(pool, id)
        .await
        .expect("작업을 읽을 수 있어야 한다")
        .remove(0)
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 업체가_확정하면_confirmed가_되고_작업이_끝난다(pool: PgPool) {
    let id = accept_booking(&pool).await;
    let adapter = Arc::new(FakeAdapter::new(IDEMPOTENT, vec![confirmed("rsv_1")]));
    let ctx = worker_with(&pool, &adapter);

    assert!(worker::process_next(&ctx).await.unwrap());

    let booking = booking(&pool, id).await;
    assert_eq!(booking.status, BookingStatus::Confirmed);
    assert_eq!(booking.provider_booking_id.as_deref(), Some("rsv_1"));
    assert_eq!(job(&pool, id).await.status, "done");

    let events = db::bookings::events(&pool, id).await.unwrap();
    let path: Vec<_> = events
        .iter()
        .map(|e| (e.to_status, e.actor.as_str()))
        .collect();
    assert_eq!(
        path,
        vec![
            (BookingStatus::Pending, "api"),
            (BookingStatus::Confirmed, "worker")
        ]
    );
    // 처리할 작업이 더 없다
    assert!(!worker::process_next(&ctx).await.unwrap());
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 업체가_거절하면_rejected가_된다(pool: PgPool) {
    let id = accept_booking(&pool).await;
    let rejected = Err(ProviderError::Rejected {
        reason: RejectReason::SlotUnavailable,
    });
    let adapter = Arc::new(FakeAdapter::new(IDEMPOTENT, vec![rejected]));

    worker::process_next(&worker_with(&pool, &adapter))
        .await
        .unwrap();

    let booking = booking(&pool, id).await;
    assert_eq!(booking.status, BookingStatus::Rejected);
    assert_eq!(booking.status_reason.as_deref(), Some("slot_unavailable"));
    assert_eq!(job(&pool, id).await.status, "done");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 일시적_실패는_나중에_다시_시도하도록_미룬다(pool: PgPool) {
    let id = accept_booking(&pool).await;
    let adapter = Arc::new(FakeAdapter::new(
        IDEMPOTENT,
        vec![Err(ProviderError::Timeout)],
    ));
    let ctx = worker_with(&pool, &adapter);

    worker::process_next(&ctx).await.unwrap();

    assert_eq!(booking(&pool, id).await.status, BookingStatus::Pending);
    let job = job(&pool, id).await;
    assert_eq!(job.status, "queued");
    assert_eq!(job.attempts, 1);
    assert_eq!(job.last_error.as_deref(), Some("응답 시간 초과"));
    // 첫 재시도는 2.5~5초 뒤라서 지금은 가져갈 작업이 없다
    assert!(job.run_at > chrono::Utc::now() + chrono::TimeDelta::seconds(2));
    assert!(!worker::process_next(&ctx).await.unwrap());
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 시도_횟수를_다_쓰면_failed가_된다(pool: PgPool) {
    let id = accept_booking(&pool).await;
    sqlx::query("UPDATE jobs SET max_attempts = 1")
        .execute(&pool)
        .await
        .unwrap();
    let adapter = Arc::new(FakeAdapter::new(
        IDEMPOTENT,
        vec![Err(ProviderError::Server { status: 503 })],
    ));

    worker::process_next(&worker_with(&pool, &adapter))
        .await
        .unwrap();

    let booking = booking(&pool, id).await;
    assert_eq!(booking.status, BookingStatus::Failed);
    assert!(
        booking
            .status_reason
            .unwrap()
            .starts_with("attempts_exhausted")
    );
    assert_eq!(job(&pool, id).await.status, "dead");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 멱등키가_없는_업체에서_타임아웃이면_다시_보내지_않고_unknown이_된다(
    pool: PgPool,
) {
    let id = accept_booking(&pool).await;
    let adapter = Arc::new(FakeAdapter::new(
        NOT_IDEMPOTENT,
        vec![
            Err(ProviderError::Timeout),
            confirmed("rsv_should_not_happen"),
        ],
    ));
    let ctx = worker_with(&pool, &adapter);

    worker::process_next(&ctx).await.unwrap();
    worker::process_next(&ctx).await.unwrap();

    assert_eq!(booking(&pool, id).await.status, BookingStatus::Unknown);
    assert_eq!(
        adapter.calls(),
        1,
        "결과를 모르는 생성 요청은 다시 보내지 않는다"
    );
    assert_eq!(job(&pool, id).await.status, "done");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 확정_기한이_지나면_업체를_부르지_않고_failed가_된다(pool: PgPool) {
    let id = accept_booking(&pool).await;
    sqlx::query("UPDATE bookings SET deadline_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let adapter = Arc::new(FakeAdapter::new(IDEMPOTENT, vec![confirmed("rsv_1")]));

    worker::process_next(&worker_with(&pool, &adapter))
        .await
        .unwrap();

    let booking = booking(&pool, id).await;
    assert_eq!(booking.status, BookingStatus::Failed);
    assert_eq!(booking.status_reason.as_deref(), Some("deadline_exceeded"));
    assert_eq!(adapter.calls(), 0);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 어댑터가_없는_업체의_예약은_failed가_된다(pool: PgPool) {
    let id = accept_booking(&pool).await;
    let ctx = WorkerContext {
        pool: pool.clone(),
        providers: Arc::new(ProviderRegistry::default()),
        config: WorkerConfig {
            concurrency: 1,
            poll_interval: Duration::from_millis(10),
            lease: Duration::from_secs(60),
        },
    };

    worker::process_next(&ctx).await.unwrap();

    let booking = booking(&pool, id).await;
    assert_eq!(booking.status, BookingStatus::Failed);
    assert_eq!(
        booking.status_reason.as_deref(),
        Some("provider_not_configured: alpha")
    );
}
