//! 워커 테스트: 가짜 어댑터로 업체 응답을 정해 두고, `process_next`로 한 단계씩 처리해 결과를 확인한다.
//!
//! 가짜 어댑터도 실제처럼 `ResilientProvider`로 감싼다. 장애 대응 계층의 동작은 providers 크레이트에서 따로 검증하므로,
//! 여기서는 호출 내 재시도를 끈 정책(`single_attempt_policy`)으로 워커의 판단만 본다.

mod common;

use std::{sync::Arc, time::Duration};

use axum::http::Method;
use booking_server::{
    registry::ProviderRegistry,
    worker::{self, WorkerConfig, WorkerContext},
};
use db::PgPool;
use domain::{BookingId, BookingStatus, RejectReason};
use providers::{
    Capabilities, CircuitBreakerPolicy, ProviderBooking, ProviderError, ProviderPolicy,
};

use common::{
    FakeAdapter, IDEMPOTENT, NOT_IDEMPOTENT, booking_body, create_client, resilient, send,
    single_attempt_policy,
};

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

fn test_config() -> WorkerConfig {
    WorkerConfig {
        concurrency: 1,
        poll_interval: Duration::from_millis(10),
        lease: Duration::from_secs(60),
        // 결과 불명 예약을 다시 조회할 때 기다리지 않는다
        unknown_recheck_interval: Duration::ZERO,
        ..WorkerConfig::default()
    }
}

/// 가짜 어댑터를 alpha 자리에 넣은 워커
fn worker_with(pool: &PgPool, adapter: &Arc<FakeAdapter>) -> WorkerContext {
    worker_with_policy(pool, adapter, single_attempt_policy())
}

fn worker_with_policy(
    pool: &PgPool,
    adapter: &Arc<FakeAdapter>,
    policy: ProviderPolicy,
) -> WorkerContext {
    let mut registry = ProviderRegistry::default();
    // Arc<FakeAdapter>는 Arc<dyn ProviderAdapter>로 자동 변환된다
    registry.insert(resilient(adapter.clone(), policy));
    WorkerContext {
        pool: pool.clone(),
        providers: Arc::new(registry),
        config: test_config(),
    }
}

fn confirmed(id: &str) -> Result<ProviderBooking, ProviderError> {
    Ok(ProviderBooking {
        provider_booking_id: id.into(),
    })
}

fn found(id: &str) -> Result<Option<ProviderBooking>, ProviderError> {
    Ok(Some(ProviderBooking {
        provider_booking_id: id.into(),
    }))
}

async fn booking(pool: &PgPool, id: BookingId) -> domain::Booking {
    db::bookings::get(pool, id)
        .await
        .expect("예약을 읽을 수 있어야 한다")
        .expect("예약이 있어야 한다")
}

/// 예약의 작업들 (등록 순서대로)
async fn jobs(pool: &PgPool, id: BookingId) -> Vec<db::jobs::JobRecord> {
    db::jobs::for_booking(pool, id)
        .await
        .expect("작업을 읽을 수 있어야 한다")
}

async fn job(pool: &PgPool, id: BookingId) -> db::jobs::JobRecord {
    jobs(pool, id).await.remove(0)
}

/// 예약 상태가 거쳐 온 길
async fn status_path(pool: &PgPool, id: BookingId) -> Vec<BookingStatus> {
    db::bookings::events(pool, id)
        .await
        .expect("이력을 읽을 수 있어야 한다")
        .iter()
        .map(|e| e.to_status)
        .collect()
}

/// 작업 종류와 상태
async fn job_summary(pool: &PgPool, id: BookingId) -> Vec<(String, String)> {
    jobs(pool, id)
        .await
        .into_iter()
        .map(|j| (j.kind, j.status))
        .collect()
}

fn pair(kind: &str, status: &str) -> (String, String) {
    (kind.to_owned(), status.to_owned())
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
async fn 서킷이_열려_있으면_시도_횟수를_쓰지_않고_미룬다(pool: PgPool) {
    let id = accept_booking(&pool).await;
    let adapter = Arc::new(FakeAdapter::new(
        IDEMPOTENT,
        vec![Err(ProviderError::Server { status: 503 })],
    ));
    // 실패 한 번에 열리는 서킷
    let policy = ProviderPolicy {
        circuit_breaker: CircuitBreakerPolicy {
            window: 1,
            min_calls: 1,
            failure_rate: 0.5,
            open_ms: 60_000,
            half_open_probes: 1,
        },
        ..single_attempt_policy()
    };
    let ctx = worker_with_policy(&pool, &adapter, policy);

    // 1차: 업체 5xx → 재시도 예약 (시도 1회 사용). 서킷이 열린다.
    worker::process_next(&ctx).await.unwrap();
    assert_eq!(job(&pool, id).await.attempts, 1);

    // 재시도 시각이 되었다고 치자. 2차: 서킷이 열려 있어 업체를 부르지 않는다.
    sqlx::query("UPDATE jobs SET run_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    worker::process_next(&ctx).await.unwrap();

    let job = job(&pool, id).await;
    assert_eq!(job.status, "queued");
    assert_eq!(job.attempts, 1, "부르지 않은 시도는 세지 않는다");
    assert_eq!(
        job.last_error.as_deref(),
        Some("서킷이 열려 있어 호출하지 않음")
    );
    assert_eq!(adapter.create_calls(), 1);
    assert_eq!(booking(&pool, id).await.status, BookingStatus::Pending);
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

    assert_eq!(booking(&pool, id).await.status, BookingStatus::Unknown);
    assert_eq!(
        adapter.create_calls(),
        1,
        "결과를 모르는 생성 요청은 다시 보내지 않는다"
    );
    // 상태 전이와 같은 트랜잭션에서 확인 작업이 등록된다
    assert_eq!(
        job_summary(&pool, id).await,
        vec![
            pair("submit_booking", "done"),
            pair("resolve_unknown", "queued")
        ]
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 결과_불명_예약을_업체에서_찾으면_confirmed가_된다(pool: PgPool) {
    let id = accept_booking(&pool).await;
    let adapter = Arc::new(
        FakeAdapter::new(NOT_IDEMPOTENT, vec![Err(ProviderError::Timeout)])
            .with_finds(vec![found("rsv_1")]),
    );
    let ctx = worker_with(&pool, &adapter);

    worker::process_next(&ctx).await.unwrap(); // submit → unknown
    worker::process_next(&ctx).await.unwrap(); // resolve → 찾음

    let booking = booking(&pool, id).await;
    assert_eq!(booking.status, BookingStatus::Confirmed);
    assert_eq!(booking.provider_booking_id.as_deref(), Some("rsv_1"));
    assert_eq!(
        status_path(&pool, id).await,
        vec![
            BookingStatus::Pending,
            BookingStatus::Unknown,
            BookingStatus::Confirmed
        ]
    );
    assert_eq!(adapter.create_calls(), 1);
    assert_eq!(
        job_summary(&pool, id).await,
        vec![
            pair("submit_booking", "done"),
            pair("resolve_unknown", "done")
        ]
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 업체에_없다는_것을_두_번_확인해야_다시_제출한다(pool: PgPool) {
    let id = accept_booking(&pool).await;
    let adapter = Arc::new(
        FakeAdapter::new(
            NOT_IDEMPOTENT,
            vec![Err(ProviderError::Timeout), confirmed("rsv_2")],
        )
        .with_finds(vec![Ok(None), Ok(None)]),
    );
    let ctx = worker_with(&pool, &adapter);

    worker::process_next(&ctx).await.unwrap(); // submit → unknown
    worker::process_next(&ctx).await.unwrap(); // resolve: 없음 (1번째)
    assert_eq!(booking(&pool, id).await.status, BookingStatus::Unknown);
    assert_eq!(
        jobs(&pool, id).await[1].payload,
        serde_json::json!({ "not_found_checks": 1 })
    );
    assert_eq!(
        adapter.create_calls(),
        1,
        "한 번 없다고 바로 다시 보내지 않는다"
    );

    worker::process_next(&ctx).await.unwrap(); // resolve: 없음 (2번째) → pending + 제출 작업
    assert_eq!(booking(&pool, id).await.status, BookingStatus::Pending);
    worker::process_next(&ctx).await.unwrap(); // 다시 제출 → 확정

    let booking = booking(&pool, id).await;
    assert_eq!(booking.status, BookingStatus::Confirmed);
    assert_eq!(booking.provider_booking_id.as_deref(), Some("rsv_2"));
    assert_eq!(
        status_path(&pool, id).await,
        vec![
            BookingStatus::Pending,
            BookingStatus::Unknown,
            BookingStatus::Pending,
            BookingStatus::Confirmed
        ]
    );
    assert_eq!((adapter.create_calls(), adapter.find_calls()), (2, 2));
    assert_eq!(
        job_summary(&pool, id).await,
        vec![
            pair("submit_booking", "done"),
            pair("resolve_unknown", "done"),
            pair("submit_booking", "done")
        ]
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 참조값_조회를_지원하지_않는_업체의_결과_불명_예약은_관리자_확인으로_넘긴다(
    pool: PgPool,
) {
    let id = accept_booking(&pool).await;
    let blind = Capabilities {
        idempotency_key: false,
        lookup_by_reference: false,
        list_bookings: false,
    };
    let adapter = Arc::new(FakeAdapter::new(blind, vec![Err(ProviderError::Timeout)]));
    let ctx = worker_with(&pool, &adapter);

    worker::process_next(&ctx).await.unwrap(); // submit → unknown
    worker::process_next(&ctx).await.unwrap(); // resolve → 조회할 수 없음

    let booking = booking(&pool, id).await;
    assert_eq!(booking.status, BookingStatus::Failed);
    assert!(
        booking
            .status_reason
            .unwrap()
            .starts_with("unresolved_unknown")
    );
    assert_eq!(adapter.find_calls(), 0);
    assert_eq!(adapter.create_calls(), 1, "자동으로 다시 제출하지 않는다");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 결과_불명_조회가_계속_실패하면_failed가_된다(pool: PgPool) {
    let id = accept_booking(&pool).await;
    let adapter = Arc::new(
        FakeAdapter::new(NOT_IDEMPOTENT, vec![Err(ProviderError::Timeout)])
            .with_finds(vec![Err(ProviderError::Server { status: 500 })]),
    );
    let ctx = worker_with(&pool, &adapter);

    worker::process_next(&ctx).await.unwrap(); // submit → unknown
    sqlx::query("UPDATE jobs SET max_attempts = 1 WHERE kind = 'resolve_unknown'")
        .execute(&pool)
        .await
        .unwrap();
    worker::process_next(&ctx).await.unwrap(); // resolve → 조회 실패, 횟수 소진

    let booking = booking(&pool, id).await;
    assert_eq!(booking.status, BookingStatus::Failed);
    assert!(
        booking
            .status_reason
            .unwrap()
            .starts_with("attempts_exhausted")
    );
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
    assert_eq!(adapter.create_calls(), 0);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 어댑터가_없는_업체의_예약은_failed가_된다(pool: PgPool) {
    let id = accept_booking(&pool).await;
    let ctx = WorkerContext {
        pool: pool.clone(),
        providers: Arc::new(ProviderRegistry::default()),
        config: test_config(),
    };

    worker::process_next(&ctx).await.unwrap();

    let booking = booking(&pool, id).await;
    assert_eq!(booking.status, BookingStatus::Failed);
    assert_eq!(
        booking.status_reason.as_deref(),
        Some("provider_not_configured: alpha")
    );
}
