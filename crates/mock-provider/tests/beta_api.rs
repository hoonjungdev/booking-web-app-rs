//! beta API를 HTTP 수준에서 검증하는 통합 테스트.
//!
//! 실제 포트를 열지 않고 라우터에 요청을 직접 넣는다 (`oneshot`).
//! (≈ ASP.NET Core의 WebApplicationFactory + TestServer)
//!
//! beta는 레거시 업체라서 업무 에러도 HTTP 200으로 온다. 그래서 상태 코드뿐 아니라 본문의 `ok` · `code`를 함께 본다.

use std::time::Duration;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Method, Request, StatusCode},
};
use chrono::{NaiveDate, NaiveDateTime, TimeDelta, Utc};
use mock_provider::{
    AppState,
    beta::model::{BookingList, BookingStatus, ShopSlots},
    catalog,
    chaos::ChaosConfig,
};
use serde_json::{Value, json};
use tower::ServiceExt;

/// 장애 주입 없이 항상 같은 결과를 내는 앱. 저장소를 직접 들여다볼 수 있게 상태도 함께 돌려준다.
fn setup() -> (Router, AppState) {
    let state = AppState::new(&ChaosConfig::passthrough());
    (mock_provider::app(state.clone()), state)
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

    /// 업무 에러 코드. 성공(`ok: true`)이면 테스트 실패
    fn error_code(&self) -> &str {
        assert_eq!(
            self.status,
            StatusCode::OK,
            "업무 에러도 HTTP 200이어야 한다"
        );
        assert_eq!(
            self.body["ok"], false,
            "실패 응답이어야 한다: {}",
            self.body
        );
        assert!(
            self.body["message"].as_str().is_some_and(|m| !m.is_empty()),
            "사람이 읽을 설명이 있어야 한다"
        );
        self.body["code"]
            .as_str()
            .expect("실패 응답에는 code가 있어야 한다")
    }
}

async fn send(app: &Router, method: Method, uri: &str, body: Option<Value>) -> Reply {
    match body {
        Some(json) => send_raw(app, method, uri, Some("application/json"), json.to_string()).await,
        None => send_raw(app, method, uri, None, String::new()).await,
    }
}

/// 본문을 글자 그대로 보낸다 (깨진 JSON 등)
async fn send_raw(
    app: &Router,
    method: Method,
    uri: &str,
    content_type: Option<&str>,
    body: String,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let request = builder
        .body(Body::from(body))
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

/// 오늘(한국 날짜)에서 `days`일 뒤. 음수면 그만큼 앞
fn kst_day(days: i64) -> NaiveDate {
    Utc::now().with_timezone(&catalog::kst()).date_naive() + TimeDelta::days(days)
}

/// `yyyyMMdd` (테스트는 서버 코드와 따로 글자를 만들어 형식을 확인한다)
fn yyyymmdd(day: NaiveDate) -> String {
    day.format("%Y%m%d").to_string()
}

fn kst_now_minute() -> String {
    Utc::now()
        .with_timezone(&catalog::kst())
        .format("%Y%m%d%H%M")
        .to_string()
}

/// 슬롯 조회 API로 찾은 열린 슬롯
struct OpenSlot {
    day: NaiveDate,
    /// 응답에 온 글자 그대로 (`yyyyMMddHHmm`, 한국 시간)
    start_at: String,
    capacity: u32,
}

/// 남은 자리가 `min_remain` 이상인 GN01 슬롯을 내일부터 일주일 안에서 slots API로 찾는다
async fn open_slot(app: &Router, min_remain: u32) -> OpenSlot {
    for days in 1..=7 {
        let day = kst_day(days);
        let reply = send(
            app,
            Method::GET,
            &format!("/beta/api/slots?shopCode=GN01&day={}", yyyymmdd(day)),
            None,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.body["ok"], true);
        let slots = reply.body["slots"]
            .as_array()
            .expect("slots는 배열이어야 한다");
        if let Some(slot) = slots
            .iter()
            .find(|s| s["remain"].as_u64() >= Some(u64::from(min_remain)))
        {
            return OpenSlot {
                day,
                start_at: slot["startAt"]
                    .as_str()
                    .expect("startAt은 문자열이어야 한다")
                    .to_owned(),
                capacity: slot["capacity"]
                    .as_u64()
                    .and_then(|c| u32::try_from(c).ok())
                    .expect("capacity는 작은 정수여야 한다"),
            };
        }
    }
    panic!("일주일 안에 남은 자리가 {min_remain} 이상인 슬롯이 하나는 있어야 한다");
}

fn book_body(start_at: &str, external_ref: &str) -> Value {
    json!({
        "shopCode": "GN01",
        "startAt": start_at,
        "people": 2,
        "guestName": "홍길동",
        "guestPhone": "010-0000-0000",
        "externalRef": external_ref
    })
}

async fn book(app: &Router, start_at: &str, external_ref: &str) -> Reply {
    send(
        app,
        Method::POST,
        "/beta/api/book",
        Some(book_body(start_at, external_ref)),
    )
    .await
}

async fn put_chaos(app: &Router, provider: &str, config: &ChaosConfig) {
    let reply = send(
        app,
        Method::PUT,
        &format!("/_chaos/{provider}"),
        Some(serde_json::to_value(config).expect("장애 설정은 JSON으로 바꿀 수 있다")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
}

fn one_request_per_second() -> ChaosConfig {
    let mut config = ChaosConfig::passthrough();
    config.rate_limit.per_second = 1;
    config.rate_limit.burst = 1;
    config
}

fn assert_no_rate_limit_headers(headers: &HeaderMap) {
    for name in [
        "x-ratelimit-limit",
        "x-ratelimit-remaining",
        "x-ratelimit-reset",
    ] {
        assert!(
            headers.get(name).is_none(),
            "beta 응답에는 {name} 헤더가 없어야 한다"
        );
    }
}

fn slots_uri(day: NaiveDate) -> String {
    format!("/beta/api/slots?shopCode=GN01&day={}", yyyymmdd(day))
}

#[tokio::test]
async fn 슬롯을_한국시간_문자열로_돌려준다() {
    let (app, _) = setup();
    let day = kst_day(1);
    let ymd = yyyymmdd(day);

    let reply = send(&app, Method::GET, &slots_uri(day), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["ok"], true);
    assert_eq!(reply.body["shopCode"], "GN01");
    assert_eq!(reply.body["day"], ymd.as_str());
    let slots = reply.body["slots"].as_array().unwrap();
    assert_eq!(slots.len(), 11);
    assert_eq!(slots[0]["startAt"], format!("{ymd}1000"));
    assert_eq!(slots[0]["endAt"], format!("{ymd}1100"));
    assert_eq!(slots[10]["startAt"], format!("{ymd}2000"));
    assert_no_rate_limit_headers(&reply.headers);

    // 한국 시간 10:00 = UTC 01:00. 해석한 값은 catalog의 UTC 슬롯과 같다.
    let parsed: ShopSlots = reply.json();
    assert_eq!(parsed.day, day);
    assert_eq!(
        parsed.slots[0].start_at.to_rfc3339(),
        format!("{day}T01:00:00+00:00")
    );
    let expected = catalog::slots_for("GN01", day);
    assert_eq!(parsed.slots.len(), expected.len());
    for (got, want) in parsed.slots.iter().zip(&expected) {
        assert_eq!(got.start_at, want.start);
        assert_eq!(got.end_at, want.end);
        assert_eq!(got.capacity, want.capacity);
        assert_eq!(got.remain, want.capacity);
    }
}

#[tokio::test]
async fn 예약하면_참조값과_기간으로_조회된다() {
    let (app, state) = setup();
    let slot = open_slot(&app, 1).await;

    let before = kst_now_minute();
    let created = book(&app, &slot.start_at, "booking-1").await;
    let after = kst_now_minute();
    assert_eq!(created.status, StatusCode::OK);
    assert_eq!(
        created.body,
        json!({ "ok": true, "bookingNo": "BK00000001", "status": "BOOKED" })
    );

    let found = send(&app, Method::GET, "/beta/api/book/by-ref/booking-1", None).await;
    assert_eq!(found.status, StatusCode::OK);
    assert_eq!(found.body["ok"], true);
    let bookings = found.body["bookings"].as_array().unwrap();
    assert_eq!(bookings.len(), 1);
    let booking = &bookings[0];
    assert_eq!(booking["bookingNo"], "BK00000001");
    assert_eq!(booking["shopCode"], "GN01");
    assert_eq!(booking["startAt"], slot.start_at.as_str());
    assert_eq!(booking["people"], 2);
    assert_eq!(booking["guestName"], "홍길동");
    assert_eq!(booking["guestPhone"], "010-0000-0000");
    assert_eq!(booking["externalRef"], "booking-1");
    assert_eq!(booking["status"], "BOOKED");
    assert_eq!(booking["cancelledAt"], Value::Null);
    // bookedAt은 접수한 순간의 한국 시간 (분 단위). 고정 길이 숫자라 글자 비교가 시각 비교와 같다.
    let booked_at = booking["bookedAt"].as_str().unwrap();
    assert!(
        before.as_str() <= booked_at && booked_at <= after.as_str(),
        "{before} <= {booked_at} <= {after}"
    );
    // endAt은 한 시간 뒤
    let start = NaiveDateTime::parse_from_str(&slot.start_at, "%Y%m%d%H%M").unwrap();
    let end = (start + TimeDelta::hours(1))
        .format("%Y%m%d%H%M")
        .to_string();
    assert_eq!(booking["endAt"], end.as_str());

    // 업체 안에는 실제 UTC 시각으로 저장된다: 한국 시간 글자 - 9시간
    let stored = state.beta.lock().unwrap().all();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].slot_start.naive_utc(),
        start - TimeDelta::hours(9)
    );
    assert_eq!(stored[0].status, BookingStatus::Booked);
    // 응답은 저장된 값을 그대로 직렬화한 것이다 (bookedAt의 초 이하는 JSON에서만 잘린다)
    assert_eq!(
        serde_json::to_value(&stored).unwrap(),
        found.body["bookings"]
    );

    // 기간 조회: 슬롯의 한국 날짜가 [fromDay, toDay] 안이면 나온다
    let ymd = yyyymmdd(slot.day);
    let same_day = send(
        &app,
        Method::GET,
        &format!("/beta/api/bookings?fromDay={ymd}&toDay={ymd}"),
        None,
    )
    .await;
    assert_eq!(same_day.status, StatusCode::OK);
    assert_eq!(same_day.body["ok"], true);
    assert_eq!(same_day.body["bookings"], found.body["bookings"]);

    let next = yyyymmdd(slot.day + TimeDelta::days(1));
    let later_days = send(
        &app,
        Method::GET,
        &format!("/beta/api/bookings?fromDay={next}&toDay={next}"),
        None,
    )
    .await;
    assert_eq!(later_days.body, json!({ "ok": true, "bookings": [] }));
}

#[tokio::test]
async fn 같은_참조값으로_두_번_보내면_예약이_두_개_생긴다() {
    let (app, _) = setup();
    let slot = open_slot(&app, 2).await;

    let first = book(&app, &slot.start_at, "booking-1").await;
    let second = book(&app, &slot.start_at, "booking-1").await;
    assert_eq!(first.body["bookingNo"], "BK00000001");
    assert_eq!(second.body["bookingNo"], "BK00000002");

    // 참조값 조회가 둘 다 돌려준다 (만든 순서). 게이트웨이는 이걸 보고 중복을 알아차려야 한다.
    let found = send(&app, Method::GET, "/beta/api/book/by-ref/booking-1", None).await;
    let numbers: Vec<&str> = found.body["bookings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["bookingNo"].as_str().unwrap())
        .collect();
    assert_eq!(numbers, ["BK00000001", "BK00000002"]);
}

#[tokio::test]
async fn 정원이_차면_200에_full() {
    let (app, _) = setup();
    let slot = open_slot(&app, 1).await;

    for i in 0..slot.capacity {
        let reply = book(&app, &slot.start_at, &format!("booking-{i}")).await;
        assert_eq!(reply.body["ok"], true);
    }
    let full = book(&app, &slot.start_at, "one-more").await;
    assert_eq!(full.error_code(), "FULL");

    let slots = send(&app, Method::GET, &slots_uri(slot.day), None).await;
    let same = slots.body["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["startAt"] == slot.start_at.as_str())
        .unwrap();
    assert_eq!(same["remain"], 0);
}

#[tokio::test]
async fn 업무_에러도_200으로_온다() {
    let (app, _) = setup();
    let slot = open_slot(&app, 1).await;
    let yesterday_ten = format!("{}1000", yyyymmdd(kst_day(-1)));
    let half_past = format!("{}30", &slot.start_at[..10]);

    let cases: Vec<(&str, &str, Value)> = vec![
        ("NO_SHOP", "shopCode", json!("XX99")),
        ("NO_SHOP", "shopCode", json!("store-001")),
        ("BAD_TIME", "startAt", json!(half_past)),
        ("BAD_TIME", "startAt", json!("2026-11-20T19:00:00+09:00")),
        ("BAD_TIME", "startAt", json!("")),
        ("PAST", "startAt", json!(yesterday_ten)),
        ("BAD_REQUEST", "people", json!(0)),
        ("BAD_REQUEST", "people", json!(9)),
        ("BAD_REQUEST", "guestName", json!("  ")),
        ("BAD_REQUEST", "guestPhone", json!("")),
        ("BAD_REQUEST", "externalRef", json!("")),
        ("BAD_REQUEST", "externalRef", json!("x".repeat(101))),
    ];
    for (code, field, value) in cases {
        let mut body = book_body(&slot.start_at, "booking-1");
        body[field] = value.clone();
        let reply = send(&app, Method::POST, "/beta/api/book", Some(body)).await;
        assert_eq!(reply.error_code(), code, "{field} = {value}");
    }

    let unknown_shop = send(
        &app,
        Method::GET,
        &format!(
            "/beta/api/slots?shopCode=store-001&day={}",
            yyyymmdd(slot.day)
        ),
        None,
    )
    .await;
    assert_eq!(unknown_shop.error_code(), "NO_SHOP");

    // 실패한 요청은 아무것도 만들지 않았다
    let all = send(&app, Method::GET, "/_inspect/beta/reservations", None).await;
    assert_eq!(all.body, json!({ "bookings": [] }));
}

#[tokio::test]
async fn 해석할_수_없는_요청도_200에_bad_request() {
    let (app, _) = setup();
    let valid = book_body("202611201900", "booking-1").to_string();
    let json = Some("application/json");

    // 깨진 JSON · 빠진 필드 · 틀린 타입 · Content-Type 없음
    let bodies = [
        (json, r#"{"shopCode":"#.to_owned()),
        (json, "{}".to_owned()),
        (json, valid.replace(r#""people":2"#, r#""people":"two""#)),
        (json, valid.replace(r#""people":2"#, r#""people":-1"#)),
        (None, valid.clone()),
    ];
    for (content_type, body) in bodies {
        let reply = send_raw(
            &app,
            Method::POST,
            "/beta/api/book",
            content_type,
            body.clone(),
        )
        .await;
        assert_eq!(reply.error_code(), "BAD_REQUEST", "{content_type:?} {body}");
    }

    // 쿼리가 없거나 형식이 틀림 (중복된 이름 포함), 경로 값이 UTF-8이 아님
    for uri in [
        "/beta/api/slots",
        "/beta/api/slots?shopCode=GN01",
        "/beta/api/slots?shopCode=&day=20261120",
        "/beta/api/slots?shopCode=GN01&day=2026-11-20",
        "/beta/api/slots?shopCode=GN01&day=20261131",
        "/beta/api/slots?shopCode=GN01&day=20261120&day=20261121",
        "/beta/api/bookings",
        "/beta/api/bookings?fromDay=20261120",
        "/beta/api/bookings?fromDay=20261121&toDay=20261120",
        "/beta/api/book/by-ref/%FF",
    ] {
        let reply = send(&app, Method::GET, uri, None).await;
        assert_eq!(reply.error_code(), "BAD_REQUEST", "{uri}");
    }
}

#[tokio::test]
async fn 취소하면_자리가_돌아오고_두_번째_취소는_already_cancelled() {
    let (app, _) = setup();
    let slot = open_slot(&app, 1).await;
    book(&app, &slot.start_at, "booking-1").await;
    let remain = |slots: &ShopSlots| slots.slots.iter().map(|s| s.remain).sum::<u32>();
    let before: ShopSlots = send(&app, Method::GET, &slots_uri(slot.day), None)
        .await
        .json();

    let first = send(&app, Method::POST, "/beta/api/book/BK00000001/cancel", None).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(
        first.body,
        json!({ "ok": true, "bookingNo": "BK00000001", "status": "CANCELLED" })
    );

    let second = send(&app, Method::POST, "/beta/api/book/BK00000001/cancel", None).await;
    assert_eq!(second.error_code(), "ALREADY_CANCELLED");
    let unknown = send(&app, Method::POST, "/beta/api/book/BK99999999/cancel", None).await;
    assert_eq!(unknown.error_code(), "NOT_FOUND");

    // 자리가 하나 돌아온다
    let after: ShopSlots = send(&app, Method::GET, &slots_uri(slot.day), None)
        .await
        .json();
    assert_eq!(remain(&after), remain(&before) + 1);

    // 취소한 예약도 참조값으로 보이고, 취소 시각이 한국 시간 글자로 붙는다
    let found = send(&app, Method::GET, "/beta/api/book/by-ref/booking-1", None).await;
    let booking = &found.body["bookings"][0];
    assert_eq!(booking["status"], "CANCELLED");
    let cancelled_at = booking["cancelledAt"].as_str().unwrap();
    assert_eq!(cancelled_at.len(), 12);
    assert!(cancelled_at.bytes().all(|b| b.is_ascii_digit()));
}

#[tokio::test]
async fn 참조값으로_찾지_못하면_not_found() {
    let (app, _) = setup();
    let reply = send(&app, Method::GET, "/beta/api/book/by-ref/nothing", None).await;
    assert_eq!(reply.error_code(), "NOT_FOUND");
}

#[tokio::test]
async fn outage면_503_maintenance이고_retry_after가_없다() {
    let (app, _) = setup();
    let reply = send(&app, Method::POST, "/_chaos/beta/preset/outage", None).await;
    assert_eq!(reply.status, StatusCode::OK);

    let reply = send(&app, Method::GET, &slots_uri(kst_day(1)), None).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["ok"], false);
    assert_eq!(reply.body["code"], "MAINTENANCE");
    assert!(reply.headers.get("retry-after").is_none());
    assert_no_rate_limit_headers(&reply.headers);

    // 다른 업체는 영향이 없고, 관리 API는 계속 응답한다
    let alpha = send(&app, Method::GET, "/alpha/v1/resources", None).await;
    assert_eq!(alpha.status, StatusCode::OK);
    let chaos = send(&app, Method::GET, "/_chaos", None).await;
    assert_eq!(chaos.body["beta"]["outage"], true);
    assert_eq!(chaos.body["alpha"]["outage"], false);
}

#[tokio::test]
async fn 호출_한도를_넘으면_503_busy와_retry_after이고_rate_limit_헤더는_없다() {
    let (app, _) = setup();
    put_chaos(&app, "beta", &one_request_per_second()).await;

    let first = send(&app, Method::GET, &slots_uri(kst_day(1)), None).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.body["ok"], true);
    assert_no_rate_limit_headers(&first.headers);

    let second = send(&app, Method::GET, &slots_uri(kst_day(1)), None).await;
    assert_eq!(second.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(second.headers["retry-after"], "1");
    assert_eq!(second.body["ok"], false);
    assert_eq!(second.body["code"], "BUSY");
    assert_no_rate_limit_headers(&second.headers);

    // 호출 한도는 업체마다 따로다
    let alpha = send(&app, Method::GET, "/alpha/v1/resources", None).await;
    assert_eq!(alpha.status, StatusCode::OK);
}

#[tokio::test]
async fn 주입한_5xx는_beta_형식이고_예약을_만들지_않는다() {
    let (app, _) = setup();
    let slot = open_slot(&app, 1).await;
    let mut config = ChaosConfig::passthrough();
    config.error.probability = 1.0;
    config.error.statuses = vec![502];
    put_chaos(&app, "beta", &config).await;

    let reply = book(&app, &slot.start_at, "booking-1").await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
    assert_eq!(reply.body["ok"], false);
    assert_eq!(reply.body["code"], "SYSTEM_ERROR");
    assert_no_rate_limit_headers(&reply.headers);

    let all = send(&app, Method::GET, "/_inspect/beta/reservations", None).await;
    assert_eq!(all.body, json!({ "bookings": [] }));
}

#[tokio::test]
async fn 쓰기_응답이_유실되어도_예약은_만들어진다() {
    let (app, state) = setup();
    let slot = open_slot(&app, 1).await;
    let mut config = ChaosConfig::passthrough();
    config.drop_after_commit_probability = 1.0;
    put_chaos(&app, "beta", &config).await;

    // 응답이 오지 않는다 (클라이언트 타임아웃). 기다리던 Future는 버려진다.
    let pending = book(&app, &slot.start_at, "booking-1");
    let timed_out = tokio::time::timeout(Duration::from_millis(300), pending).await;
    assert!(timed_out.is_err(), "응답이 오지 않아야 한다");

    // 그래도 업체에는 예약이 있다 (결과 불명). 게이트웨이는 다시 보내지 말고 참조값으로 확인해야 한다.
    assert_eq!(state.beta.lock().unwrap().all().len(), 1);
    put_chaos(&app, "beta", &ChaosConfig::passthrough()).await;
    let found = send(&app, Method::GET, "/beta/api/book/by-ref/booking-1", None).await;
    assert_eq!(found.body["bookings"][0]["bookingNo"], "BK00000001");
}

#[tokio::test]
async fn 관리_api도_beta를_다룬다() {
    let (app, _) = setup();
    let slot = open_slot(&app, 1).await;
    book(&app, &slot.start_at, "booking-1").await;

    let all = send(&app, Method::GET, "/_inspect/beta/reservations", None).await;
    assert_eq!(all.status, StatusCode::OK);
    assert!(all.body.get("reservations").is_none());
    let bookings = all.json::<BookingList>().bookings;
    assert_eq!(bookings.len(), 1);
    assert_eq!(bookings[0].booking_no, "BK00000001");

    // 업체 쪽에서 임의로 취소한다 (대사 작업이 찾아내야 할 상황)
    let uri = "/_inspect/beta/reservations/BK00000001/cancel";
    let cancelled = send(&app, Method::POST, uri, None).await;
    assert_eq!(cancelled.status, StatusCode::OK);
    assert_eq!(cancelled.body["bookingNo"], "BK00000001");
    assert_eq!(cancelled.body["status"], "CANCELLED");
    let again = send(&app, Method::POST, uri, None).await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.body["code"], "ALREADY_CANCELLED");
    let unknown = send(
        &app,
        Method::POST,
        "/_inspect/beta/reservations/BK99999999/cancel",
        None,
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.body["code"], "NOT_FOUND");

    let found = send(&app, Method::GET, "/beta/api/book/by-ref/booking-1", None).await;
    assert_eq!(found.body["bookings"][0]["status"], "CANCELLED");

    // reset: 예약이 사라지고 예약 번호도 처음부터, 장애 설정은 healthy로
    send(&app, Method::POST, "/_chaos/beta/preset/outage", None).await;
    let reply = send(&app, Method::POST, "/_reset", None).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let all = send(&app, Method::GET, "/_inspect/beta/reservations", None).await;
    assert_eq!(all.body, json!({ "bookings": [] }));
    let chaos = send(&app, Method::GET, "/_chaos", None).await;
    assert_eq!(chaos.body["beta"]["outage"], false);
    assert_eq!(chaos.body["beta"]["latency_ms"]["max"], 120);

    put_chaos(&app, "beta", &ChaosConfig::passthrough()).await;
    let again = book(&app, &slot.start_at, "booking-2").await;
    assert_eq!(again.body["bookingNo"], "BK00000001");
}

#[tokio::test]
async fn alpha의_응답_형식은_그대로다() {
    let (app, _) = setup();
    put_chaos(&app, "alpha", &one_request_per_second()).await;
    put_chaos(&app, "beta", &one_request_per_second()).await;

    // 호출 한도: alpha는 429 + X-RateLimit-*, beta는 503 + 헤더 없음
    let first = send(&app, Method::GET, "/alpha/v1/resources", None).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.headers["x-ratelimit-limit"], "1");
    assert_eq!(first.headers["x-ratelimit-remaining"], "0");
    let limited = send(&app, Method::GET, "/alpha/v1/resources", None).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers["retry-after"], "1");
    assert!(limited.headers.get("x-ratelimit-reset").is_some());
    assert_eq!(limited.body["error"]["code"], "rate_limited");

    send(&app, Method::GET, &slots_uri(kst_day(1)), None).await;
    let busy = send(&app, Method::GET, &slots_uri(kst_day(1)), None).await;
    assert_eq!(busy.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(busy.body["code"], "BUSY");

    // 주입한 5xx: alpha는 `{"error": {...}}`
    let mut failing = ChaosConfig::passthrough();
    failing.error.probability = 1.0;
    failing.error.statuses = vec![500];
    put_chaos(&app, "alpha", &failing).await;
    let error = send(&app, Method::GET, "/alpha/v1/resources", None).await;
    assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error.body["error"]["code"], "internal_error");
    assert!(error.headers.get("x-ratelimit-limit").is_some());

    // outage: alpha는 `service_unavailable`
    send(&app, Method::POST, "/_chaos/alpha/preset/outage", None).await;
    let outage = send(&app, Method::GET, "/alpha/v1/resources", None).await;
    assert_eq!(outage.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(outage.body["error"]["code"], "service_unavailable");
    assert!(outage.headers.get("retry-after").is_none());
}
