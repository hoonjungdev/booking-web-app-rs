//! booking-server 통합 테스트 공용 코드

#![allow(dead_code)] // 테스트 파일마다 쓰는 헬퍼가 달라서, 일부 파일에서는 안 쓰는 함수가 생긴다

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Method, Request, StatusCode},
};
use booking_server::{
    api::{self, ApiState, BookingResponse},
    auth,
};
use chrono::{Days, Utc};
use db::PgPool;
use domain::{BookingId, BookingStatus, ClientId, ProviderId};
use providers::{
    AdapterReply, CallRecorder, Capabilities, CircuitBreakerPolicy, CreateBooking, ProviderAdapter,
    ProviderBooking, ProviderError, ProviderPolicy, RateLimitPolicy, ResilientProvider,
    RetryPolicy,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

pub struct TestClient {
    pub id: ClientId,
    pub api_key: String,
}

/// API 클라이언트를 만들고 키를 발급한다 (`booking-server create-client`와 같은 일)
pub async fn create_client(pool: &PgPool, name: &str) -> TestClient {
    let api_key = auth::generate_api_key();
    let id = ClientId::from_uuid(Uuid::now_v7());
    db::clients::create(pool, id, name, &auth::hash_api_key(&api_key))
        .await
        .expect("클라이언트를 만들 수 있어야 한다");
    TestClient { id, api_key }
}

pub fn app(pool: &PgPool) -> Router {
    api::router(ApiState { pool: pool.clone() })
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
}

impl Reply {
    pub fn booking(&self) -> BookingResponse {
        serde_json::from_value(self.body.clone()).expect("예약 응답 형식이어야 한다")
    }
}

/// 라우터에 요청을 직접 넣는다 (실제 포트를 열지 않음)
pub async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("테스트 요청은 올바르게 만들어진다");

    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("axum Router는 실패하지 않는다");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("응답 본문을 읽을 수 있어야 한다");
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    Reply {
        status,
        headers,
        body,
    }
}

/// 예약 생성 요청 본문
pub fn booking_body(slot_start: &str) -> Value {
    json!({
        "provider": "alpha",
        "resource_id": "store-001",
        "slot_start": slot_start,
        "party_size": 2,
        "customer": { "name": "홍길동", "phone": "010-0000-0000" }
    })
}

/// mock 업체 alpha에서 내일 정원이 남아 있는 첫 슬롯의 시작 시각 (RFC 3339 UTC)
pub fn open_slot() -> String {
    let tomorrow = Utc::now()
        .with_timezone(&mock_provider::catalog::kst())
        .date_naive()
        .checked_add_days(Days::new(1))
        .expect("내일 날짜는 항상 계산할 수 있다");
    mock_provider::catalog::slots_for("store-001", tomorrow)
        .into_iter()
        .find(|slot| slot.capacity > 0)
        .expect("내일 열린 슬롯이 하나는 있어야 한다")
        .start
        .to_rfc3339()
}

/// 예약이 기대한 상태가 될 때까지 조회 API로 확인한다 (워커가 백그라운드에서 처리하는 동안 기다리기)
pub async fn wait_for_status(
    app: &Router,
    api_key: &str,
    booking_id: &str,
    expected: BookingStatus,
    timeout: Duration,
) -> BookingResponse {
    let started = Instant::now();
    loop {
        let reply = send(
            app,
            Method::GET,
            &format!("/v1/bookings/{booking_id}"),
            &[("x-api-key", api_key)],
            None,
        )
        .await;
        let booking = reply.booking();
        if booking.status == expected {
            return booking;
        }
        assert!(
            started.elapsed() < timeout,
            "{timeout:?} 안에 {expected}가 되지 않았다 (현재 {})",
            booking.status
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 정해 둔 응답을 순서대로 돌려주는 가짜 어댑터 (≈ C#의 Moq로 만든 test double)
pub struct FakeAdapter {
    capabilities: Capabilities,
    creates: Mutex<VecDeque<Result<ProviderBooking, ProviderError>>>,
    finds: Mutex<VecDeque<Result<Option<ProviderBooking>, ProviderError>>>,
    create_calls: AtomicUsize,
    find_calls: AtomicUsize,
}

impl FakeAdapter {
    /// `creates`: 예약 생성 호출에 차례로 돌려줄 응답
    pub fn new(
        capabilities: Capabilities,
        creates: Vec<Result<ProviderBooking, ProviderError>>,
    ) -> Self {
        Self {
            capabilities,
            creates: Mutex::new(creates.into()),
            finds: Mutex::new(VecDeque::new()),
            create_calls: AtomicUsize::new(0),
            find_calls: AtomicUsize::new(0),
        }
    }

    /// 참조값 조회 호출에 차례로 돌려줄 응답
    pub fn with_finds(self, finds: Vec<Result<Option<ProviderBooking>, ProviderError>>) -> Self {
        *self
            .finds
            .lock()
            .expect("테스트 중에는 잠금이 오염되지 않는다") = finds.into();
        self
    }

    pub fn create_calls(&self) -> usize {
        self.create_calls.load(Ordering::SeqCst)
    }

    pub fn find_calls(&self) -> usize {
        self.find_calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for FakeAdapter {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    async fn create_booking(&self, _request: &CreateBooking) -> AdapterReply<ProviderBooking> {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        self.creates
            .lock()
            .expect("테스트 중에는 잠금이 오염되지 않는다")
            .pop_front()
            .unwrap_or(Err(ProviderError::Server { status: 599 }))
            .into()
    }

    async fn find_booking(&self, _reference: BookingId) -> AdapterReply<Option<ProviderBooking>> {
        self.find_calls.fetch_add(1, Ordering::SeqCst);
        self.finds
            .lock()
            .expect("테스트 중에는 잠금이 오염되지 않는다")
            .pop_front()
            .unwrap_or(Err(ProviderError::Server { status: 599 }))
            .into()
    }
}

/// 워커 판단만 보려고 장애 대응 계층을 사실상 끈 정책: 호출 내 재시도 없음, 넉넉한 rate limit, 잘 열리지 않는 서킷
pub fn single_attempt_policy() -> ProviderPolicy {
    ProviderPolicy {
        retry: RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        },
        rate_limit: RateLimitPolicy {
            per_second: 10_000,
            burst: 10_000,
            max_wait_ms: 100,
        },
        circuit_breaker: CircuitBreakerPolicy {
            window: 1_000,
            min_calls: 1_000,
            ..CircuitBreakerPolicy::default()
        },
        ..ProviderPolicy::default()
    }
}

/// 어댑터를 alpha 업체로서 장애 대응 계층으로 감싼다 (호출 기록은 버림)
pub fn resilient(
    adapter: Arc<dyn ProviderAdapter>,
    policy: ProviderPolicy,
) -> Arc<ResilientProvider> {
    Arc::new(ResilientProvider::new(
        ProviderId::parse("alpha").expect("올바른 업체 ID"),
        adapter,
        policy,
        CallRecorder::discard(),
    ))
}

pub const IDEMPOTENT: Capabilities = Capabilities {
    idempotency_key: true,
    lookup_by_reference: true,
    list_bookings: true,
};

pub const NOT_IDEMPOTENT: Capabilities = Capabilities {
    idempotency_key: false,
    lookup_by_reference: true,
    list_bookings: false,
};
