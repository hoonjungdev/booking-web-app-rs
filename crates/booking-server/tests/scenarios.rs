//! 장애 시나리오 통합 테스트 (docs/03-resilience.md §10, 로드맵 Phase 3 완료 기준)
//!
//! 실제 구성 그대로 돌린다: 예약 API → DB → 워커 루프 → `ResilientProvider` → alpha 어댑터 → mock 업체(장애 주입),
//! 그리고 호출 기록 태스크 → `provider_calls`. 모든 시나리오에서 **업체에 같은 참조값의 예약이 2건 이상 없는지**(이중 예약 0건) 검사한다.
//!
//! 테스트가 빨리 끝나도록 시간 관련 값(타임아웃, 서킷 open 시간, 지연)을 실제 기본값보다 10배쯤 줄였다.
//! mock 업체의 장애는 확률로 일어나므로, "재시도가 있었다" 같은 검사는 일어나지 않을 확률이 0.1% 미만이 되게 건수를 잡았다.

mod common;

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    http::{Method, StatusCode},
};
use booking_server::{
    recorder,
    registry::ProviderRegistry,
    worker::{self, JobBackoff, WorkerConfig, WorkerContext},
};
use chrono::{DateTime, Days, Utc};
use db::PgPool;
use domain::{Booking, BookingId, BookingStatus, ProviderId};
use mock_provider::{
    alpha::model::ReservationQuery,
    catalog,
    chaos::{ChaosConfig, LatencyRange},
};
use providers::{
    AdapterReply, BulkheadPolicy, CallRecord, Capabilities, CircuitBreakerPolicy, CircuitState,
    CreateBooking, ProviderAdapter, ProviderBooking, ProviderError, ProviderPolicy,
    RateLimitPolicy, ResilientProvider, RetryPolicy, call_recorder,
};
use rand::RngExt;
use serde_json::json;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use common::{create_client, send};

/// 모든 예약이 끝나기를 기다리는 최대 시간
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

// ───────────────────────── 시나리오 ─────────────────────────

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 정상_업체에서는_모두_확정되고_429를_받지_않는다(pool: PgPool) {
    // healthy 프리셋: 20~120ms 지연, 업체 한도 초당 20회(burst 10). 우리 limiter는 그보다 낮게 둔다.
    let chaos = mock_provider::chaos::Preset::Healthy.config();
    let policy = ProviderPolicy {
        rate_limit: RateLimitPolicy {
            per_second: 10,
            burst: 5,
            max_wait_ms: 1_000,
        },
        ..test_policy()
    };
    let harness = Harness::with_mock(pool, chaos, policy).await;

    let ids = harness.submit(15).await;
    let bookings = harness.wait_settled(&ids).await;

    assert_all(&bookings, BookingStatus::Confirmed);
    harness.assert_matches_mock(&bookings);
    let calls = harness.finish().await;
    assert_eq!(
        calls.count("success"),
        15,
        "예약마다 성공한 생성 호출은 하나"
    );
    assert_eq!(
        calls.count("rate_limited"),
        0,
        "우리 limiter가 업체 한도 안에서 호출한다"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 간헐적_5xx는_호출_내_재시도로_흡수한다(pool: PgPool) {
    let mut chaos = quiet();
    chaos.error.probability = 0.4;
    chaos.error.statuses = vec![500, 502, 503];
    let harness = Harness::with_mock(pool, chaos, test_policy()).await;

    let ids = harness.submit(20).await;
    let bookings = harness.wait_settled(&ids).await;

    assert_all(&bookings, BookingStatus::Confirmed);
    harness.assert_matches_mock(&bookings);
    let calls = harness.finish().await;
    // 5xx가 한 번도 없을 확률: 0.6^20 ≈ 0.004%
    assert!(calls.count("server_error") > 0, "{calls:?}");
    assert!(
        calls.retried > 0,
        "호출 내 재시도가 있었어야 한다: {calls:?}"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 응답이_느려지면_서킷이_열리고_회복되면_닫힌다(pool: PgPool) {
    // 모든 요청이 1초 걸린다 → 시도 타임아웃(300ms)
    let mut slow = quiet();
    slow.slow.probability = 1.0;
    slow.slow.latency_ms = 1_000;
    let harness = Harness::with_mock(pool, slow, test_policy()).await;
    let ids = harness.submit(8).await;

    harness.wait_for_circuit(CircuitState::Open).await;
    for booking in harness.bookings(&ids).await {
        assert_eq!(
            booking.status,
            BookingStatus::Pending,
            "서킷이 열려도 예약은 보류된다"
        );
    }

    // 업체가 회복되면 반만 열린 상태의 시험 호출이 성공하고, 밀린 예약이 처리된다
    harness.set_chaos(quiet());
    let bookings = harness.wait_settled(&ids).await;

    assert_all(&bookings, BookingStatus::Confirmed);
    assert_eq!(harness.alpha().circuit_state(), CircuitState::Closed);
    harness.assert_matches_mock(&bookings);
    let calls = harness.finish().await;
    assert!(calls.count("timeout") >= 10, "{calls:?}");
    assert!(calls.count("circuit_open") > 0, "{calls:?}");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 업체_한도보다_낮게_호출하면_429_없이_모두_처리된다(pool: PgPool) {
    // throttled 프리셋과 같은 업체 한도 (초당 5회). 우리 limiter는 초당 4회.
    let mut throttled = quiet();
    throttled.rate_limit.per_second = 5;
    throttled.rate_limit.burst = 5;
    let policy = ProviderPolicy {
        rate_limit: RateLimitPolicy {
            per_second: 4,
            burst: 2,
            max_wait_ms: 2_000,
        },
        ..test_policy()
    };
    let harness = Harness::with_mock(pool, throttled, policy).await;

    let ids = harness.submit(10).await;
    let bookings = harness.wait_settled(&ids).await;

    assert_all(&bookings, BookingStatus::Confirmed);
    harness.assert_matches_mock(&bookings);
    let calls = harness.finish().await;
    assert_eq!(calls.count("rate_limited"), 0, "{calls:?}");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 업체가_429를_보내면_pause했다가_재개한다(pool: PgPool) {
    // 업체 한도(초당 2회)보다 우리 limiter가 훨씬 느슨하다 → 429 → 업체 전체 pause
    let mut throttled = quiet();
    throttled.rate_limit.per_second = 2;
    throttled.rate_limit.burst = 2;
    let harness = Harness::with_mock(pool, throttled, test_policy()).await;

    let ids = harness.submit(8).await;
    let bookings = harness.wait_settled(&ids).await;

    assert_all(&bookings, BookingStatus::Confirmed);
    harness.assert_matches_mock(&bookings);
    let calls = harness.finish().await;
    assert!(calls.count("rate_limited") > 0, "{calls:?}");
    assert!(
        calls.count("throttled") > 0,
        "pause 중인 호출은 업체에 보내지 않는다: {calls:?}"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 응답이_유실되어도_같은_멱등키로_재시도해_이중_예약이_없다(
    pool: PgPool,
) {
    // 쓰기 요청의 40%를 처리한 뒤 응답하지 않는다 (lossy 프리셋을 더 세게)
    let mut lossy = quiet();
    lossy.drop_after_commit_probability = 0.4;
    let harness = Harness::with_mock(pool, lossy, test_policy()).await;

    let ids = harness.submit(20).await;
    let bookings = harness.wait_settled(&ids).await;

    assert_all(&bookings, BookingStatus::Confirmed);
    harness.assert_matches_mock(&bookings);
    assert_eq!(harness.mock_reservations().len(), 20);
    let calls = harness.finish().await;
    // 응답 유실이 한 번도 없을 확률: 0.6^20 ≈ 0.004%
    assert!(calls.count("timeout") > 0, "{calls:?}");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 업체가_멈추면_예약을_보류하고_기한이_지난_것만_실패시킨다(
    pool: PgPool,
) {
    let mut outage = quiet();
    outage.outage = true;
    let harness = Harness::with_mock(pool, outage, test_policy()).await;
    let ids = harness.submit(8).await;

    harness.wait_for_circuit(CircuitState::Open).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    for booking in harness.bookings(&ids).await {
        assert_eq!(booking.status, BookingStatus::Pending, "장애 중에는 보류");
    }

    // 장애가 길어져 한 예약의 확정 기한이 지났다
    let expired = ids[0];
    sqlx::query("UPDATE bookings SET deadline_at = now() WHERE id = $1")
        .bind(expired.as_uuid())
        .execute(&harness.pool)
        .await
        .expect("기한을 바꿀 수 있어야 한다");
    let failed = harness
        .wait_until(expired, |b| b.status != BookingStatus::Pending)
        .await;
    assert_eq!(failed.status, BookingStatus::Failed);
    assert_eq!(failed.status_reason.as_deref(), Some("deadline_exceeded"));

    // 복구되면 나머지는 자동으로 처리된다
    harness.set_chaos(quiet());
    let bookings = harness.wait_settled(&ids[1..]).await;

    assert_all(&bookings, BookingStatus::Confirmed);
    harness.assert_matches_mock(&bookings);
    assert_eq!(
        harness.mock_reservations().len(),
        7,
        "기한이 지난 예약은 보내지 않았다"
    );
    let calls = harness.finish().await;
    assert!(calls.count("server_error") > 0, "{calls:?}");
    assert!(calls.count("circuit_open") > 0, "{calls:?}");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 멱등키가_없는_업체는_결과_불명을_조회로_확인해_이중_예약이_없다(
    pool: PgPool,
) {
    // beta(Phase 6)처럼 멱등키가 없는 업체를 메모리로 흉내 낸다. 요청의 25%는 업체에 닿지 않고,
    // 25%는 업체가 처리한 뒤 응답이 사라진다. 우리 쪽에서는 둘 다 타임아웃으로 보인다.
    // (두 경로를 반드시 거치도록 첫 번째와 두 번째 요청은 정해진 방식으로 실패한다)
    let simulated = Arc::new(LossyProvider::default());
    let harness = Harness::with_adapter(pool, simulated.clone(), test_policy()).await;

    let ids = harness.submit(15).await;
    let bookings = harness.wait_settled(&ids).await;

    assert_all(&bookings, BookingStatus::Confirmed);
    let stored = simulated.reservations();
    assert_no_double_booking(stored.iter().map(|(reference, _)| *reference));
    for booking in &bookings {
        let at_provider = stored
            .iter()
            .find(|(reference, _)| *reference == booking.id)
            .map(|(_, id)| id.as_str());
        assert_eq!(booking.provider_booking_id.as_deref(), at_provider);
    }
    let mut went_unknown = 0;
    for id in &ids {
        let path = db::bookings::events(&harness.pool, *id)
            .await
            .expect("이력을 읽을 수 있어야 한다");
        if path.iter().any(|e| e.to_status == BookingStatus::Unknown) {
            went_unknown += 1;
        }
    }
    assert!(went_unknown >= 2, "결과 불명을 거친 예약이 있어야 한다");
    assert!(
        simulated.creates() > 15,
        "업체에 닿지 않은 요청은 없음을 두 번 확인한 뒤 다시 제출되었다"
    );

    let calls = harness.finish().await;
    assert!(
        calls.by_operation.get("find_booking").copied().unwrap_or(0) > 0,
        "{calls:?}"
    );
}

// ───────────────────────── 준비 코드 ─────────────────────────

/// 테스트용 정책: 실제 기본값(docs/03-resilience.md §3)의 시간을 10분의 1쯤으로 줄였다
fn test_policy() -> ProviderPolicy {
    ProviderPolicy {
        attempt_timeout_ms: 300,
        call_budget_ms: 3_000,
        retry: RetryPolicy {
            max_attempts: 3,
            base_delay_ms: 20,
            max_delay_ms: 200,
        },
        rate_limit: RateLimitPolicy {
            per_second: 1_000,
            burst: 100,
            max_wait_ms: 500,
        },
        bulkhead: BulkheadPolicy {
            max_concurrency: 8,
            max_wait_ms: 200,
        },
        circuit_breaker: CircuitBreakerPolicy {
            window: 20,
            min_calls: 10,
            failure_rate: 0.5,
            open_ms: 500,
            half_open_probes: 3,
        },
    }
}

/// 장애 없이 조금(5~20ms) 느린 업체
fn quiet() -> ChaosConfig {
    ChaosConfig {
        latency_ms: LatencyRange { min: 5, max: 20 },
        ..ChaosConfig::passthrough()
    }
}

fn worker_config() -> WorkerConfig {
    WorkerConfig {
        concurrency: 4,
        poll_interval: Duration::from_millis(20),
        lease: Duration::from_secs(30),
        backoff: JobBackoff {
            base: Duration::from_millis(100),
            max: Duration::from_secs(1),
        },
        unknown_recheck_interval: Duration::from_millis(100),
    }
}

/// 시나리오 하나에 필요한 것: mock 업체, 워커 루프, 호출 기록 태스크, 예약 API
struct Harness {
    pool: PgPool,
    app: Router,
    api_key: String,
    mock: Option<mock_provider::AppState>,
    alpha: Arc<ResilientProvider>,
    shutdown: CancellationToken,
    worker: JoinHandle<()>,
    recorder: JoinHandle<()>,
}

/// 시나리오가 끝난 뒤 `provider_calls`에서 센 호출 수
#[derive(Debug)]
struct CallStats {
    by_outcome: BTreeMap<String, i64>,
    by_operation: BTreeMap<String, i64>,
    /// 호출 내 재시도(시도 번호 2 이상) 수
    retried: i64,
}

impl CallStats {
    fn count(&self, outcome: &str) -> i64 {
        self.by_outcome.get(outcome).copied().unwrap_or(0)
    }
}

impl Harness {
    /// mock 업체를 띄우고, DB의 alpha 설정(주소 · 정책)을 바꾼 뒤 실제 설정 경로(`ProviderRegistry::load`)로 시작한다
    async fn with_mock(pool: PgPool, chaos: ChaosConfig, policy: ProviderPolicy) -> Self {
        let mock = mock_provider::AppState::new(&chaos);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("임의 포트에 바인드할 수 있어야 한다");
        let addr = listener.local_addr().expect("주소를 알 수 있어야 한다");
        let app = mock_provider::app(mock.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });

        sqlx::query("UPDATE providers SET base_url = $1, policy = $2 WHERE id = 'alpha'")
            .bind(format!("http://{addr}/alpha/v1"))
            .bind(serde_json::to_value(&policy).expect("정책은 JSON으로 바뀐다"))
            .execute(&pool)
            .await
            .expect("업체 설정을 바꿀 수 있어야 한다");

        let (call_recorder, calls) = call_recorder(recorder::CHANNEL_CAPACITY);
        let registry = ProviderRegistry::load(&pool, call_recorder)
            .await
            .expect("업체 파이프라인을 만들 수 있어야 한다");
        Self::start(pool, registry, calls, Some(mock)).await
    }

    /// mock 서버 대신 메모리 어댑터를 alpha 자리에 넣어 시작한다
    async fn with_adapter(
        pool: PgPool,
        adapter: Arc<dyn ProviderAdapter>,
        policy: ProviderPolicy,
    ) -> Self {
        let (call_recorder, calls) = call_recorder(recorder::CHANNEL_CAPACITY);
        let mut registry = ProviderRegistry::default();
        registry.insert(Arc::new(ResilientProvider::new(
            alpha_id(),
            adapter,
            policy,
            call_recorder,
        )));
        Self::start(pool, registry, calls, None).await
    }

    async fn start(
        pool: PgPool,
        registry: ProviderRegistry,
        calls: mpsc::Receiver<CallRecord>,
        mock: Option<mock_provider::AppState>,
    ) -> Self {
        let alpha = registry
            .get(&alpha_id())
            .expect("alpha가 등록되어 있어야 한다");
        let ctx = WorkerContext {
            pool: pool.clone(),
            providers: Arc::new(registry),
            config: worker_config(),
        };
        let shutdown = CancellationToken::new();
        let worker = tokio::spawn(worker::run(ctx, shutdown.clone()));
        let recorder = tokio::spawn(recorder::run(pool.clone(), calls));
        let client = create_client(&pool, "scenario").await;
        Self {
            app: common::app(&pool),
            api_key: client.api_key,
            pool,
            mock,
            alpha,
            shutdown,
            worker,
            recorder,
        }
    }

    fn alpha(&self) -> &ResilientProvider {
        &self.alpha
    }

    fn set_chaos(&self, chaos: ChaosConfig) {
        self.mock
            .as_ref()
            .expect("mock 업체로 시작한 시나리오다")
            .chaos
            .set(mock_provider::Provider::Alpha, chaos);
    }

    fn mock_reservations(&self) -> Vec<mock_provider::alpha::model::Reservation> {
        self.mock
            .as_ref()
            .expect("mock 업체로 시작한 시나리오다")
            .alpha
            .lock()
            .expect("잠금이 오염되지 않는다")
            .search(&ReservationQuery::default())
    }

    /// 예약 `n`건을 서로 다른 멱등키로 접수한다. 정원이 차서 거절되지 않도록 슬롯마다 정원만큼만 담는다.
    async fn submit(&self, n: usize) -> Vec<BookingId> {
        let mut ids = Vec::with_capacity(n);
        for (index, (resource, slot)) in open_seats(n).into_iter().enumerate() {
            let body = json!({
                "provider": "alpha",
                "resource_id": resource,
                "slot_start": slot.to_rfc3339(),
                "party_size": 2,
                "customer": { "name": "홍길동", "phone": "010-0000-0000" }
            });
            let key = format!("scenario-{index}");
            let reply = send(
                &self.app,
                Method::POST,
                "/v1/bookings",
                &[("x-api-key", &self.api_key), ("idempotency-key", &key)],
                Some(&body),
            )
            .await;
            assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.body);
            ids.push(reply.booking().id);
        }
        ids
    }

    async fn bookings(&self, ids: &[BookingId]) -> Vec<Booking> {
        let mut bookings = Vec::with_capacity(ids.len());
        for id in ids {
            let booking = db::bookings::get(&self.pool, *id)
                .await
                .expect("예약을 읽을 수 있어야 한다")
                .expect("예약이 있어야 한다");
            bookings.push(booking);
        }
        bookings
    }

    /// 모든 예약이 처리 중(`pending`, `unknown`)이 아닌 상태가 될 때까지 기다린다
    async fn wait_settled(&self, ids: &[BookingId]) -> Vec<Booking> {
        let started = Instant::now();
        loop {
            let bookings = self.bookings(ids).await;
            let busy: Vec<_> = bookings
                .iter()
                .filter(|b| matches!(b.status, BookingStatus::Pending | BookingStatus::Unknown))
                .map(|b| (b.id, b.status))
                .collect();
            if busy.is_empty() {
                return bookings;
            }
            assert!(
                started.elapsed() < SETTLE_TIMEOUT,
                "{SETTLE_TIMEOUT:?} 안에 끝나지 않은 예약: {busy:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn wait_until(&self, id: BookingId, done: impl Fn(&Booking) -> bool) -> Booking {
        let started = Instant::now();
        loop {
            let booking = self.bookings(&[id]).await.remove(0);
            if done(&booking) {
                return booking;
            }
            assert!(
                started.elapsed() < SETTLE_TIMEOUT,
                "조건을 만족하지 않았다: {:?}",
                booking.status
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn wait_for_circuit(&self, expected: CircuitState) {
        let started = Instant::now();
        while self.alpha.circuit_state() != expected {
            assert!(
                started.elapsed() < SETTLE_TIMEOUT,
                "서킷이 {expected:?}가 되지 않았다"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 확정된 예약마다 mock 업체에 그 예약이 정확히 하나 있고, 업체 예약 ID가 일치하는지 확인한다
    fn assert_matches_mock(&self, bookings: &[Booking]) {
        let reservations = self.mock_reservations();
        assert_no_double_booking(reservations.iter().filter_map(|r| {
            r.client_reference
                .as_deref()
                .and_then(|reference| reference.parse().ok())
                .map(BookingId::from_uuid)
        }));
        for booking in bookings
            .iter()
            .filter(|b| b.status == BookingStatus::Confirmed)
        {
            let reference = booking.id.to_string();
            let at_provider = reservations
                .iter()
                .find(|r| r.client_reference.as_deref() == Some(reference.as_str()))
                .map(|r| r.id.as_str());
            assert_eq!(
                booking.provider_booking_id.as_deref(),
                at_provider,
                "확정된 예약 {reference}는 업체 예약과 연결되어야 한다"
            );
        }
    }

    /// 워커를 멈추고 남은 호출 기록이 저장될 때까지 기다린 뒤, 호출 수를 센다
    async fn finish(self) -> CallStats {
        let Self {
            pool,
            alpha,
            shutdown,
            worker,
            recorder,
            ..
        } = self;
        shutdown.cancel();
        worker.await.expect("워커가 정상 종료되어야 한다");
        // 업체 파이프라인(기록을 보내는 쪽)이 모두 사라져야 기록 태스크가 남은 것을 저장하고 끝난다
        drop(alpha);
        recorder.await.expect("기록 태스크가 정상 종료되어야 한다");

        let rows: Vec<(String, String, i32)> =
            sqlx::query_as("SELECT outcome, operation, attempt FROM provider_calls")
                .fetch_all(&pool)
                .await
                .expect("호출 기록을 읽을 수 있어야 한다");
        let mut stats = CallStats {
            by_outcome: BTreeMap::new(),
            by_operation: BTreeMap::new(),
            retried: 0,
        };
        for (outcome, operation, attempt) in rows {
            *stats.by_outcome.entry(outcome).or_default() += 1;
            *stats.by_operation.entry(operation).or_default() += 1;
            if attempt > 1 {
                stats.retried += 1;
            }
        }
        stats
    }
}

fn alpha_id() -> ProviderId {
    ProviderId::parse("alpha").expect("올바른 업체 ID")
}

fn assert_all(bookings: &[Booking], expected: BookingStatus) {
    for booking in bookings {
        assert_eq!(
            booking.status, expected,
            "예약 {} (사유: {:?})",
            booking.id, booking.status_reason
        );
    }
}

/// 이중 예약 0건: 같은 참조값(우리 예약 ID)의 업체 예약이 2건 이상 없다
fn assert_no_double_booking(references: impl Iterator<Item = BookingId>) {
    let mut counts: HashMap<BookingId, usize> = HashMap::new();
    for reference in references {
        *counts.entry(reference).or_default() += 1;
    }
    let doubled: Vec<_> = counts.iter().filter(|(_, count)| **count > 1).collect();
    assert!(doubled.is_empty(), "이중 예약 발견: {doubled:?}");
}

/// mock 업체에서 정원이 남은 자리 `n`개 (내일부터, 세 매장의 슬롯을 정원만큼 반복)
fn open_seats(n: usize) -> Vec<(&'static str, DateTime<Utc>)> {
    let today = Utc::now().with_timezone(&catalog::kst()).date_naive();
    let mut seats = Vec::new();
    for days in 1..=3 {
        let date = today
            .checked_add_days(Days::new(days))
            .expect("날짜를 계산할 수 있어야 한다");
        for resource in catalog::RESOURCES {
            for slot in catalog::slots_for(resource.id, date) {
                for _ in 0..slot.capacity {
                    seats.push((resource.id, slot.start));
                }
            }
        }
    }
    assert!(seats.len() >= n, "정원이 모자란다: {}", seats.len());
    seats.truncate(n);
    seats
}

/// 멱등키가 없는 업체를 메모리로 흉내 낸다 (beta와 같은 능력: 멱등키 없음, 참조값 조회 가능)
#[derive(Default)]
struct LossyProvider {
    /// (우리 참조값, 업체 예약 ID)
    reservations: Mutex<Vec<(BookingId, String)>>,
    creates: AtomicUsize,
}

impl LossyProvider {
    fn reservations(&self) -> Vec<(BookingId, String)> {
        self.reservations
            .lock()
            .expect("잠금이 오염되지 않는다")
            .clone()
    }

    fn creates(&self) -> usize {
        self.creates.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for LossyProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            idempotency_key: false,
            lookup_by_reference: true,
            list_bookings: true,
        }
    }

    async fn create_booking(&self, request: &CreateBooking) -> AdapterReply<ProviderBooking> {
        let number = self.creates.fetch_add(1, Ordering::SeqCst);
        // 0번째 요청: 업체에 닿지 않음, 1번째: 처리 후 응답 유실, 그다음부터는 무작위
        let roll: f64 = match number {
            0 => 0.0,
            1 => 0.3,
            _ => rand::rng().random(),
        };
        if roll < 0.25 {
            // 요청이 업체에 닿지 않았지만 우리는 알 수 없다
            return Err(ProviderError::Timeout).into();
        }
        let id = format!("sim_{number}");
        self.reservations
            .lock()
            .expect("잠금이 오염되지 않는다")
            .push((request.reference, id.clone()));
        if roll < 0.5 {
            // 업체는 만들었는데 응답이 사라졌다
            return Err(ProviderError::Timeout).into();
        }
        Ok(ProviderBooking {
            provider_booking_id: id,
        })
        .into()
    }

    async fn find_booking(&self, reference: BookingId) -> AdapterReply<Option<ProviderBooking>> {
        let found = self
            .reservations
            .lock()
            .expect("잠금이 오염되지 않는다")
            .iter()
            .find(|(r, _)| *r == reference)
            .map(|(_, id)| ProviderBooking {
                provider_booking_id: id.clone(),
            });
        Ok(found).into()
    }
}
