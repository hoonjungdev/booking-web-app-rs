//! 워커 테스트: 가짜 어댑터로 업체 응답을 정해 두고, `process_next`로 한 단계씩 처리해 결과를 확인한다.
//!
//! 가짜 어댑터도 실제처럼 `ResilientProvider`로 감싼다. 장애 대응 계층의 동작은 providers 크레이트에서 따로 검증하므로,
//! 여기서는 호출 내 재시도를 끈 정책(`single_attempt_policy`)으로 워커의 판단만 본다.

mod common;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::http::{Method, StatusCode};
use booking_server::{
    registry::ProviderRegistry,
    worker::{self, WorkerConfig, WorkerContext},
};
use chrono::{DateTime, Utc};
use db::{PgPool, TraceRef};
use domain::{BookingId, BookingStatus, ProviderId, RejectReason};
use providers::{
    AdapterReply, Capabilities, CircuitBreakerPolicy, CreateBooking, ListedBooking, ListedStatus,
    ProviderAdapter, ProviderBooking, ProviderError, ProviderPolicy,
};

use common::{
    FakeAdapter, IDEMPOTENT, NOT_IDEMPOTENT, booking_body, create_client, resilient, send,
    single_attempt_policy,
};

/// 예약 하나를 API로 접수하고 ID를 돌려준다
async fn accept_booking(pool: &PgPool) -> BookingId {
    let client = create_client(pool, "app").await;
    let slot = (Utc::now() + chrono::TimeDelta::days(1)).to_rfc3339();
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
    assert!(job.run_at > Utc::now() + chrono::TimeDelta::seconds(2));
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

// ───────────────────────── Phase 6: 보냄 표시 ─────────────────────────

/// 생성 요청을 받는 순간 DB의 "보냄" 표시를 읽어 두는 업체. 표시가 보내기 **전에** 커밋되는지 확인한다.
struct ProbeAdapter {
    pool: PgPool,
    capabilities: Capabilities,
    /// 생성 요청을 받을 때 본 `jobs.sent_at`
    seen: Mutex<Option<Option<DateTime<Utc>>>>,
}

#[async_trait::async_trait]
impl ProviderAdapter for ProbeAdapter {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    async fn create_booking(&self, request: &CreateBooking) -> AdapterReply<ProviderBooking> {
        let sent_at: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT sent_at FROM jobs WHERE booking_id = $1 AND kind = 'submit_booking'",
        )
        .bind(request.reference.as_uuid())
        .fetch_one(&self.pool)
        .await
        .expect("작업을 읽을 수 있어야 한다");
        *self.seen.lock().expect("잠금이 오염되지 않는다") = Some(sent_at);
        Ok(ProviderBooking {
            provider_booking_id: "rsv_probe".into(),
        })
        .into()
    }

    async fn find_booking(&self, _reference: BookingId) -> AdapterReply<Option<ProviderBooking>> {
        Ok(None).into()
    }
}

async fn probe(pool: &PgPool, capabilities: Capabilities) -> Option<Option<DateTime<Utc>>> {
    let id = accept_booking(pool).await;
    let adapter = Arc::new(ProbeAdapter {
        pool: pool.clone(),
        capabilities,
        seen: Mutex::new(None),
    });
    let mut registry = ProviderRegistry::default();
    registry.insert(resilient(adapter.clone(), single_attempt_policy()));
    let ctx = WorkerContext {
        pool: pool.clone(),
        providers: Arc::new(registry),
        config: test_config(),
    };
    assert!(
        worker::process_next(&ctx)
            .await
            .expect("작업을 처리할 수 있어야 한다")
    );
    assert_eq!(booking(pool, id).await.status, BookingStatus::Confirmed);
    assert_eq!(
        job(pool, id).await.sent_at,
        None,
        "결과를 기록하면 표시가 지워진다"
    );
    adapter.seen.lock().expect("잠금이 오염되지 않는다").take()
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 멱등키가_없는_업체에는_보내기_전에_보냄_표시를_커밋한다(
    pool: PgPool,
) {
    let seen = probe(&pool, NOT_IDEMPOTENT).await;
    assert!(
        matches!(seen, Some(Some(_))),
        "업체가 요청을 받은 순간 표시가 이미 DB에 있어야 한다: {seen:?}"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 멱등키가_있는_업체에는_보냄_표시를_남기지_않는다(pool: PgPool) {
    // 같은 멱등키로 다시 보내도 업체가 중복을 막으므로 DB 쓰기를 한 번 아낀다
    assert_eq!(probe(&pool, IDEMPOTENT).await, Some(None));
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 보낸_뒤_결과를_남기지_못하고_멈춘_작업은_다시_보내지_않고_결과_불명으로_넘긴다(
    pool: PgPool,
) {
    let id = accept_booking(&pool).await;
    // 워커 A: 작업을 가져가 보냄 표시를 남기고 보낸 직후 죽었다 (결과를 기록하지 못함)
    let claimed = db::jobs::claim_next(&pool, Duration::from_secs(60), &db::jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        db::jobs::mark_sent(&mut conn, claimed.lease())
            .await
            .unwrap()
    );
    sqlx::query("UPDATE jobs SET locked_until = now() - interval '1 second' WHERE id = $1")
        .bind(claimed.id)
        .execute(&pool)
        .await
        .unwrap();

    // 워커 B: lease가 끝난 작업을 다시 가져간다
    let adapter = Arc::new(
        FakeAdapter::new(NOT_IDEMPOTENT, vec![confirmed("rsv_dup")])
            .with_finds(vec![found("rsv_1")]),
    );
    let ctx = worker_with(&pool, &adapter);
    assert!(worker::process_next(&ctx).await.unwrap());

    assert_eq!(adapter.create_calls(), 0, "다시 보내면 이중 예약 위험");
    let unknown = booking(&pool, id).await;
    assert_eq!(unknown.status, BookingStatus::Unknown);
    assert_eq!(
        unknown.status_reason.as_deref(),
        Some(worker::UNRECORDED_SEND)
    );
    assert_eq!(
        job_summary(&pool, id).await,
        vec![
            pair("submit_booking", "done"),
            pair("resolve_unknown", "queued")
        ]
    );

    // 업체에서 조회해 보니 워커 A가 보낸 예약이 있었다
    assert!(worker::process_next(&ctx).await.unwrap());
    let resolved = booking(&pool, id).await;
    assert_eq!(resolved.status, BookingStatus::Confirmed);
    assert_eq!(resolved.provider_booking_id.as_deref(), Some("rsv_1"));
    assert_eq!(adapter.create_calls(), 0);
}

// ───────────────────────── Phase 6: 취소 흐름 ─────────────────────────

/// 예약 하나를 API로 접수하고 (예약 ID, API 키)를 돌려준다
async fn accept_with_key(pool: &PgPool) -> (BookingId, String) {
    let client = create_client(pool, "canceller").await;
    let slot = (Utc::now() + chrono::TimeDelta::days(1)).to_rfc3339();
    let id = send(
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
    .id;
    (id, client.api_key)
}

async fn request_cancel(pool: &PgPool, api_key: &str, id: BookingId) -> common::Reply {
    send(
        &common::app(pool),
        Method::POST,
        &format!("/v1/bookings/{id}/cancel"),
        &[("x-api-key", api_key)],
        None,
    )
    .await
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 확정된_예약을_취소하면_업체에서_취소해_cancelled가_된다(pool: PgPool) {
    let (id, key) = accept_with_key(&pool).await;
    let adapter =
        Arc::new(FakeAdapter::new(IDEMPOTENT, vec![confirmed("rsv_1")]).with_cancels(vec![Ok(())]));
    let ctx = worker_with(&pool, &adapter);
    worker::process_next(&ctx).await.unwrap();
    assert_eq!(booking(&pool, id).await.status, BookingStatus::Confirmed);

    let reply = request_cancel(&pool, &key, id).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.body);
    assert_eq!(reply.booking().status, BookingStatus::CancelRequested);
    // 같은 요청을 다시 보내도 작업은 하나다 (취소는 멱등)
    let again = request_cancel(&pool, &key, id).await;
    assert_eq!(again.status, StatusCode::ACCEPTED);
    assert_eq!(
        job_summary(&pool, id).await,
        vec![
            pair("submit_booking", "done"),
            pair("cancel_booking", "queued")
        ]
    );

    assert!(worker::process_next(&ctx).await.unwrap());
    assert_eq!(adapter.cancel_calls(), 1);
    assert_eq!(
        status_path(&pool, id).await,
        vec![
            BookingStatus::Pending,
            BookingStatus::Confirmed,
            BookingStatus::CancelRequested,
            BookingStatus::Cancelled
        ]
    );
    let done = request_cancel(&pool, &key, id).await;
    assert_eq!(done.status, StatusCode::ACCEPTED);
    assert_eq!(done.booking().status, BookingStatus::Cancelled);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 확정되지_않은_예약이나_다른_클라이언트의_예약은_취소할_수_없다(
    pool: PgPool,
) {
    let (id, key) = accept_with_key(&pool).await;
    let pending = request_cancel(&pool, &key, id).await;
    assert_eq!(pending.status, StatusCode::CONFLICT);
    assert!(pending.body["detail"].as_str().unwrap().contains("처리 중"));

    let stranger = create_client(&pool, "stranger").await;
    let other = request_cancel(&pool, &stranger.api_key, id).await;
    assert_eq!(other.status, StatusCode::NOT_FOUND);
    assert_eq!(booking(&pool, id).await.status, BookingStatus::Pending);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 취소는_결과를_모르는_실패_뒤에도_다시_보낸다(pool: PgPool) {
    let (id, key) = accept_with_key(&pool).await;
    let adapter = Arc::new(
        FakeAdapter::new(NOT_IDEMPOTENT, vec![confirmed("rsv_1")])
            .with_cancels(vec![Err(ProviderError::Timeout), Ok(())]),
    );
    let ctx = worker_with(&pool, &adapter);
    worker::process_next(&ctx).await.unwrap();
    request_cancel(&pool, &key, id).await;

    worker::process_next(&ctx).await.unwrap();
    assert_eq!(
        booking(&pool, id).await.status,
        BookingStatus::CancelRequested,
        "타임아웃은 결과 불명이 아니라 재시도다 (취소는 다시 보내도 결과가 같다)"
    );
    let cancel_job = jobs(&pool, id).await.remove(1);
    assert_eq!(
        (cancel_job.kind.as_str(), cancel_job.status.as_str()),
        ("cancel_booking", "queued")
    );
    sqlx::query("UPDATE jobs SET run_at = now() WHERE id = $1")
        .bind(cancel_job.id)
        .execute(&pool)
        .await
        .unwrap();
    worker::process_next(&ctx).await.unwrap();
    assert_eq!(booking(&pool, id).await.status, BookingStatus::Cancelled);
    assert_eq!(adapter.cancel_calls(), 2);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 업체가_취소를_받지_않으면_cancel_failed가_되고_관리자_재시도로_다시_취소한다(
    pool: PgPool,
) {
    let (id, key) = accept_with_key(&pool).await;
    let adapter = Arc::new(
        FakeAdapter::new(IDEMPOTENT, vec![confirmed("rsv_1")]).with_cancels(vec![
            Err(ProviderError::Rejected {
                reason: RejectReason::BookingNotFound,
            }),
            Ok(()),
        ]),
    );
    let ctx = worker_with(&pool, &adapter);
    worker::process_next(&ctx).await.unwrap();
    request_cancel(&pool, &key, id).await;
    worker::process_next(&ctx).await.unwrap();

    let failed = booking(&pool, id).await;
    assert_eq!(failed.status, BookingStatus::CancelFailed);
    assert_eq!(failed.status_reason.as_deref(), Some("booking_not_found"));
    let dead = jobs(&pool, id).await.remove(1);
    assert_eq!(
        (dead.kind.as_str(), dead.status.as_str()),
        ("cancel_booking", "dead")
    );
    // 클라이언트가 다시 취소를 요청해도 이미 받은 취소라 202 (관리자를 기다린다)
    let again = request_cancel(&pool, &key, id).await;
    assert_eq!(again.status, StatusCode::ACCEPTED);
    assert_eq!(again.booking().status, BookingStatus::CancelFailed);

    // 관리자가 업체와 확인한 뒤 다시 시도한다: 같은 dead 작업을 되살려 취소를 다시 보낸다
    let outcome =
        booking_server::api::admin::retry_booking(&pool, id, &TraceRef::default(), "admin:kim")
            .await
            .unwrap();
    assert_eq!(
        outcome.booking.unwrap().status,
        BookingStatus::CancelRequested
    );
    assert_eq!(outcome.job.id, dead.id, "dead 작업을 같은 행으로 되살린다");
    assert_eq!(outcome.job.kind, "cancel_booking");

    worker::process_next(&ctx).await.unwrap();
    assert_eq!(booking(&pool, id).await.status, BookingStatus::Cancelled);
    let events = db::bookings::events(&pool, id).await.unwrap();
    let retry = events
        .iter()
        .find(|e| e.from_status == Some(BookingStatus::CancelFailed))
        .unwrap();
    assert_eq!(
        (retry.to_status, retry.actor.as_str()),
        (BookingStatus::CancelRequested, "admin:kim")
    );
}

// ───────────────────────── Phase 6: 대사 ─────────────────────────

/// alpha의 대사 작업을 지금 실행하도록 등록하고 작업 ID를 돌려준다
async fn schedule_reconcile_now(pool: &PgPool) -> i64 {
    let mut conn = pool.acquire().await.expect("DB 연결");
    db::jobs::schedule_reconcile(
        &mut conn,
        &ProviderId::parse("alpha").expect("업체 ID"),
        Duration::ZERO,
        None,
    )
    .await
    .expect("대사 작업 등록")
    .expect("대기 중인 대사 작업이 없었다")
}

/// 대사 작업의 상태들 (등록 순서대로)
async fn reconcile_jobs(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT status FROM jobs WHERE kind = 'reconcile_provider' ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("대사 작업을 읽을 수 있어야 한다")
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 대사가_업체에서_확정된_실패_예약을_찾으면_confirmed로_바꾸고_dead_작업을_정리한다(
    pool: PgPool,
) {
    let id = accept_booking(&pool).await;
    let listed = ListedBooking {
        provider_booking_id: "rsv_9".into(),
        reference: Some(id.to_string()),
        slot_start: booking(&pool, id).await.request.slot_start,
        status: ListedStatus::Confirmed,
    };
    let adapter = Arc::new(
        FakeAdapter::new(
            IDEMPOTENT,
            vec![Err(ProviderError::Client {
                status: 400,
                body: "bad".into(),
            })],
        )
        .with_lists(vec![Ok(vec![listed])]),
    );
    let ctx = worker_with(&pool, &adapter);
    // 업체가 예상하지 못한 4xx로 답해 포기했다: 예약은 failed, 작업은 dead (그런데 업체에는 예약이 만들어져 있었다)
    worker::process_next(&ctx).await.unwrap();
    assert_eq!(booking(&pool, id).await.status, BookingStatus::Failed);
    assert_eq!(
        job_summary(&pool, id).await,
        vec![pair("submit_booking", "dead")]
    );

    schedule_reconcile_now(&pool).await;
    assert!(worker::process_next(&ctx).await.unwrap());

    assert_eq!(adapter.list_calls(), 1);
    let fixed = booking(&pool, id).await;
    assert_eq!(fixed.status, BookingStatus::Confirmed);
    assert_eq!(fixed.provider_booking_id.as_deref(), Some("rsv_9"));
    let events = db::bookings::events(&pool, id).await.unwrap();
    let last = events.last().unwrap();
    assert_eq!(
        (last.from_status, last.to_status, last.actor.as_str()),
        (
            Some(BookingStatus::Failed),
            BookingStatus::Confirmed,
            "reconciler"
        )
    );
    assert_eq!(
        job_summary(&pool, id).await,
        vec![pair("submit_booking", "done")],
        "묻힌 작업을 정리해 dead 알림이 그친다"
    );
    let (kind, resolved_by): (String, Option<String>) =
        sqlx::query_as("SELECT kind, resolved_by FROM reconciliation_issues WHERE booking_id = $1")
            .bind(id.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (kind.as_str(), resolved_by.as_deref()),
        ("status_mismatch", Some("reconciler")),
        "자동으로 바로잡은 것도 처리 완료 이슈로 남긴다"
    );
    assert_eq!(
        reconcile_jobs(&pool).await,
        vec!["done".to_owned(), "queued".to_owned()],
        "끝나면 다음 대사를 등록한다"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 업체_호출_한도에_여유가_없으면_대사를_미루고_업체를_부르지_않는다(
    pool: PgPool,
) {
    let adapter = Arc::new(
        FakeAdapter::new(IDEMPOTENT, vec![])
            .with_finds(vec![Ok(None), Ok(None)])
            .with_lists(vec![Ok(vec![])]),
    );
    let policy = ProviderPolicy {
        rate_limit: providers::RateLimitPolicy {
            per_second: 1,
            burst: 2,
            max_wait_ms: 100,
            adaptive: false,
        },
        ..single_attempt_policy()
    };
    let ctx = worker_with_policy(&pool, &adapter, policy);
    let alpha = ctx
        .providers
        .get(&ProviderId::parse("alpha").unwrap())
        .unwrap();
    // 사용자가 기다리는 호출이 버킷의 토큰을 다 썼다
    for _ in 0..2 {
        alpha
            .find_booking(
                BookingId::from_uuid(uuid::Uuid::now_v7()),
                providers::CallContext::default(),
            )
            .await
            .unwrap();
    }
    assert!(!alpha.has_spare_capacity());

    let job_id = schedule_reconcile_now(&pool).await;
    assert!(worker::process_next(&ctx).await.unwrap());
    assert_eq!(adapter.list_calls(), 0, "업체를 부르지 않는다");
    let mut conn = pool.acquire().await.unwrap();
    let job = db::jobs::get(&mut conn, job_id).await.unwrap().unwrap();
    assert_eq!(job.status, "queued", "미뤘다가 다시 돈다");
    assert_eq!(job.attempts, 0, "미룬 것은 시도 횟수로 세지 않는다");
}
