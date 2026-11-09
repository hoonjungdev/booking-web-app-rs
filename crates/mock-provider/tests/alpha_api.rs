//! alpha API를 HTTP 수준에서 검증하는 통합 테스트.
//!
//! 실제 포트를 열지 않고 라우터에 요청을 직접 넣는다 (`oneshot`).
//! (≈ ASP.NET Core의 WebApplicationFactory + TestServer)

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Method, Request, StatusCode},
};
use chrono::{Days, Utc};
use mock_provider::{
    AppState,
    alpha::model::{Availability, Reservation, ReservationList, ReservationStatus},
    catalog,
    chaos::ChaosConfig,
};
use serde_json::{Value, json};
use tower::ServiceExt;

/// 장애 주입 없이 항상 같은 결과를 내는 앱
fn app() -> Router {
    mock_provider::app(AppState::new(&ChaosConfig::passthrough()))
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Reply {
    fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_value(self.body.clone()).expect("응답 본문을 해석할 수 있어야 한다")
    }
}

async fn send(app: &Router, method: Method, uri: &str, body: Option<Value>) -> Reply {
    send_with(app, method, uri, body, &[]).await
}

async fn send_with(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
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
    .expect("테스트 요청은 항상 올바르게 만들어진다");

    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("axum Router는 실패하지 않는다 (에러 타입이 Infallible)");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("메모리 안의 응답 본문은 읽을 수 있다");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    Reply {
        status,
        headers,
        body,
    }
}

/// 내일(한국 날짜) 정원이 1 이상인 첫 슬롯을 availability API로 찾는다
async fn open_slot(app: &Router) -> (String, u32) {
    let tomorrow = Utc::now()
        .with_timezone(&catalog::kst())
        .date_naive()
        .checked_add_days(Days::new(1))
        .expect("내일 날짜는 항상 계산할 수 있다");
    let reply = send(
        app,
        Method::GET,
        &format!("/alpha/v1/availability?resource_id=store-001&date={tomorrow}"),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let availability: Availability = reply.json();
    let slot = availability
        .slots
        .into_iter()
        .find(|s| s.remaining > 0)
        .expect("내일 열린 슬롯이 하나는 있어야 한다");
    (slot.start.to_rfc3339(), slot.capacity)
}

fn reservation_body(slot_start: &str, reference: &str) -> Value {
    json!({
        "resource_id": "store-001",
        "slot_start": slot_start,
        "party_size": 2,
        "customer": { "name": "홍길동", "phone": "010-0000-0000" },
        "client_reference": reference
    })
}

#[tokio::test]
async fn 자원_목록을_돌려준다() {
    let reply = send(&app(), Method::GET, "/alpha/v1/resources", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["resources"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn 예약을_만들고_조회한다() {
    let app = app();
    let (slot, _) = open_slot(&app).await;

    let created = send(
        &app,
        Method::POST,
        "/alpha/v1/reservations",
        Some(reservation_body(&slot, "booking-1")),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let created: Reservation = created.json();
    assert_eq!(created.status, ReservationStatus::Confirmed);

    let fetched = send(
        &app,
        Method::GET,
        &format!("/alpha/v1/reservations/{}", created.id),
        None,
    )
    .await;
    assert_eq!(fetched.status, StatusCode::OK);
    assert_eq!(fetched.json::<Reservation>(), created);

    let by_reference = send(
        &app,
        Method::GET,
        "/alpha/v1/reservations?client_reference=booking-1",
        None,
    )
    .await;
    assert_eq!(
        by_reference.json::<ReservationList>().reservations,
        vec![created]
    );
}

#[tokio::test]
async fn 같은_멱등키로_다시_보내면_예약이_하나만_생긴다() {
    let app = app();
    let (slot, _) = open_slot(&app).await;
    let key = [("idempotency-key", "booking-1")];

    let first = send_with(
        &app,
        Method::POST,
        "/alpha/v1/reservations",
        Some(reservation_body(&slot, "booking-1")),
        &key,
    )
    .await;
    let second = send_with(
        &app,
        Method::POST,
        "/alpha/v1/reservations",
        Some(reservation_body(&slot, "booking-1")),
        &key,
    )
    .await;

    assert_eq!(first.status, StatusCode::CREATED);
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(first.json::<Reservation>(), second.json::<Reservation>());

    let all = send(&app, Method::GET, "/_inspect/alpha/reservations", None).await;
    assert_eq!(all.json::<ReservationList>().reservations.len(), 1);
}

#[tokio::test]
async fn 같은_멱등키에_다른_본문이면_422() {
    let app = app();
    let (slot, _) = open_slot(&app).await;
    let key = [("idempotency-key", "booking-1")];

    send_with(
        &app,
        Method::POST,
        "/alpha/v1/reservations",
        Some(reservation_body(&slot, "booking-1")),
        &key,
    )
    .await;
    let mut different = reservation_body(&slot, "booking-1");
    different["party_size"] = json!(3);
    let reply = send_with(
        &app,
        Method::POST,
        "/alpha/v1/reservations",
        Some(different),
        &key,
    )
    .await;

    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["error"]["code"], "idempotency_key_mismatch");
}

#[tokio::test]
async fn 정원이_차면_409() {
    let app = app();
    let (slot, capacity) = open_slot(&app).await;

    for i in 0..capacity {
        let reply = send(
            &app,
            Method::POST,
            "/alpha/v1/reservations",
            Some(reservation_body(&slot, &format!("booking-{i}"))),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
    }
    let full = send(
        &app,
        Method::POST,
        "/alpha/v1/reservations",
        Some(reservation_body(&slot, "one-more")),
    )
    .await;

    assert_eq!(full.status, StatusCode::CONFLICT);
    assert_eq!(full.body["error"]["code"], "slot_unavailable");
}

#[tokio::test]
async fn 취소는_여러_번_해도_같은_결과다() {
    let app = app();
    let (slot, _) = open_slot(&app).await;
    let created: Reservation = send(
        &app,
        Method::POST,
        "/alpha/v1/reservations",
        Some(reservation_body(&slot, "booking-1")),
    )
    .await
    .json();
    let uri = format!("/alpha/v1/reservations/{}", created.id);

    let first = send(&app, Method::DELETE, &uri, None).await;
    let second = send(&app, Method::DELETE, &uri, None).await;

    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(
        first.json::<Reservation>().status,
        ReservationStatus::Cancelled
    );
    assert_eq!(first.json::<Reservation>(), second.json::<Reservation>());
}

#[tokio::test]
async fn 경계가_아닌_시각이나_없는_자원은_거절한다() {
    let app = app();
    let (slot, _) = open_slot(&app).await;
    let off_boundary = slot.replace(":00:00", ":30:00");

    let reply = send(
        &app,
        Method::POST,
        "/alpha/v1/reservations",
        Some(reservation_body(&off_boundary, "x")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.body["error"]["code"], "invalid_slot");

    let mut unknown = reservation_body(&slot, "x");
    unknown["resource_id"] = json!("store-999");
    let reply = send(&app, Method::POST, "/alpha/v1/reservations", Some(unknown)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn outage면_503이고_관리_api는_계속_응답한다() {
    let app = app();
    let reply = send(&app, Method::POST, "/_chaos/alpha/preset/outage", None).await;
    assert_eq!(reply.status, StatusCode::OK);

    let reply = send(&app, Method::GET, "/alpha/v1/resources", None).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);

    let reply = send(&app, Method::GET, "/_chaos", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["alpha"]["outage"], true);
}

#[tokio::test]
async fn 호출_한도를_넘으면_429와_retry_after() {
    let app = app();
    let mut config = ChaosConfig::passthrough();
    config.rate_limit.per_second = 1;
    config.rate_limit.burst = 1;
    let reply = send(
        &app,
        Method::PUT,
        "/_chaos/alpha",
        Some(serde_json::to_value(&config).unwrap()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);

    let first = send(&app, Method::GET, "/alpha/v1/resources", None).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.headers["x-ratelimit-limit"], "1");
    assert_eq!(first.headers["x-ratelimit-remaining"], "0");

    let second = send(&app, Method::GET, "/alpha/v1/resources", None).await;
    assert_eq!(second.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(second.headers["retry-after"], "1");
    assert_eq!(second.body["error"]["code"], "rate_limited");
}

#[tokio::test]
async fn 잘못된_장애_설정은_400() {
    let reply = send(
        &app(),
        Method::PUT,
        "/_chaos/alpha",
        Some(json!({ "hang_probability": 2.0 })),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reset하면_예약이_사라지고_healthy로_돌아간다() {
    let app = app();
    let (slot, _) = open_slot(&app).await;
    send(
        &app,
        Method::POST,
        "/alpha/v1/reservations",
        Some(reservation_body(&slot, "booking-1")),
    )
    .await;

    let reply = send(&app, Method::POST, "/_reset", None).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);

    let all = send(&app, Method::GET, "/_inspect/alpha/reservations", None).await;
    assert!(all.json::<ReservationList>().reservations.is_empty());
    let chaos = send(&app, Method::GET, "/_chaos", None).await;
    assert_eq!(chaos.body["alpha"]["latency_ms"]["max"], 120);
}
