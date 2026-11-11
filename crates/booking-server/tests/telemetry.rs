//! 관측 통합 테스트 (docs/05-observability-and-analytics.md, 로드맵 Phase 4 완료 기준)
//!
//! Grafana를 띄우지 않고, 메모리에 모은 span · 메트릭으로 다음을 확인한다 (`telemetry::testing`).
//! - 예약 한 건의 trace가 접수 요청 → (link) → 작업 → 업체 호출 → **mock 업체 내부**까지 이어진다
//! - 응답 헤더 · 오류 본문 · DB(상태 이력, 호출 기록)에 trace ID가 남는다
//! - 지표가 문서의 이름 · 라벨 · 버킷으로 기록된다
//! - 고객 정보가 span 속성과 로그로 새지 않는다 (업체가 오류 본문에 되돌려 준 경우까지)
//! - 응답을 받기 전에 버려진 요청도 span과 지표에 남는다
//!
//! 실제 구성과 다른 점은 내보내는 곳(OTLP 대신 메모리)뿐이다. mock 업체도 실제 HTTP 서버로 띄우므로
//! `traceparent` 헤더가 정말로 네트워크를 건너간다.
//!
//! 계측기와 subscriber는 프로세스 전역이라, 한 프로세스에서 여러 테스트가 섞이면 값이 겹친다.
//! 그래서 이 파일에는 테스트 함수를 하나만 두고 그 안에서 순서대로 확인한다.

mod common;

use std::{sync::Arc, time::Duration};

use axum::http::{Method, StatusCode};
use booking_server::{
    DEFAULT_LOG_FILTER, metrics,
    registry::ProviderRegistry,
    worker::{self, WorkerConfig, WorkerContext},
};
use chrono::Utc;
use db::PgPool;
use domain::{BookingId, BookingStatus, Customer, ProviderId};
use mock_provider::chaos::{ChaosConfig, LatencyRange};
use providers::{
    CallContext, CallRecord, CallRecorder, CreateBooking, ProviderBooking, ProviderError,
    ProviderPolicy, ResilientProvider, RetryPolicy,
};
use serde_json::json;
use telemetry::testing::{Captured, MetricPoint, SpanView};
use tokio::sync::mpsc;
use tracing::Instrument;
use uuid::Uuid;

use common::{
    FakeAdapter, IDEMPOTENT, Reply, booking_body, create_client, send, single_attempt_policy,
};

/// `telemetry::SECONDS_BUCKETS` 뒤에 긴 시간용 경계를 덧붙인 것 (booking_server::metrics의 LONG_SECONDS_BUCKETS)
const LONG_BUCKET_TAIL: [f64; 6] = [30.0, 60.0, 120.0, 300.0, 600.0, 1800.0];

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 예약_한_건의_trace가_업체_내부까지_이어지고_지표가_기록된다(
    pool: PgPool,
) {
    // 계측기는 처음 쓸 때의 전역 meter provider에 묶이므로, 다른 무엇보다 먼저 설치한다.
    // 실제 서버의 기본 로그 필터를 그대로 써서 "실제 설정에서 보이는 span"을 검증한다.
    let captured = telemetry::testing::install(DEFAULT_LOG_FILTER);

    // ── 준비: 지연을 주입하는 mock 업체(실제 HTTP 서버) + 워커 + 예약 API ──
    let (mock, mock_url) = start_mock(&pool).await;
    // 오류 본문에 고객 정보를 되돌려 주는 업체 `echo` (8번에서 쓴다). 예약을 접수하려면 DB에 업체가 있어야 한다.
    let (echo, mut echo_calls) = echo_provider(&pool).await;
    let (call_recorder, mut calls) = providers::call_recorder(16);
    let mut registry = ProviderRegistry::load(&pool, call_recorder)
        .await
        .expect("업체 파이프라인을 만들 수 있어야 한다");
    // `_flaky_calls`: 받는 쪽을 테스트가 끝날 때까지 들고만 있는다 (읽지 않으므로 1칸짜리 채널이 금방 가득 찬다)
    let (flaky, _flaky_calls) = flaky_provider();
    registry.insert(flaky.clone());
    registry.insert(echo);
    let registry = Arc::new(registry);
    metrics::register_provider_gauges(&registry);
    let ctx = WorkerContext {
        pool: pool.clone(),
        providers: registry,
        config: WorkerConfig::default(),
    };
    let app = common::app(&pool);
    let client = create_client(&pool, "app").await;
    let auth = ("x-api-key", client.api_key.as_str());

    // ── 1. 접수 요청 ──
    let accepted = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &[auth, ("idempotency-key", "order-1")],
        Some(&booking_body(&common::open_slot())),
    )
    .await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED);
    let booking_id = accepted.booking().id;
    let request_trace = trace_header(&accepted).expect("응답에 X-Trace-Id가 있어야 한다");

    let spans = captured.spans();
    let server = span_in(&spans, &request_trace, "POST /v1/bookings");
    assert_eq!(server.kind, "server");
    assert_eq!(server.parent_span_id, None);
    assert_eq!(server.attribute("http.response.status_code"), Some("202"));
    let handler = span_in(&spans, &request_trace, "booking.accept");
    assert_eq!(
        handler.parent_span_id.as_deref(),
        Some(server.span_id.as_str())
    );
    assert_eq!(
        handler.attribute("booking_id"),
        Some(booking_id.to_string().as_str()),
        "예약 ID로 trace를 찾을 수 있어야 한다"
    );
    assert_eq!(handler.attribute("provider"), Some("alpha"));
    let db_span = span_in(&spans, &request_trace, "db.bookings.accept");
    assert_eq!(
        db_span.parent_span_id.as_deref(),
        Some(handler.span_id.as_str())
    );
    for span in spans.iter().filter(|span| span.trace_id == request_trace) {
        assert_no_customer_data(span);
    }

    // ── 2. 워커가 작업을 처리한다: 새 trace + 접수 요청으로 가는 link ──
    assert!(
        worker::process_next(&ctx)
            .await
            .expect("작업을 처리할 수 있어야 한다")
    );
    let job = wait_for_span(captured, |span| {
        span.name == "job.submit_booking"
            && span.attribute("booking_id") == Some(booking_id.to_string().as_str())
    })
    .await;
    let job_trace = job.trace_id.clone();
    assert_ne!(job_trace, request_trace, "작업은 접수 요청과 다른 trace다");
    assert_eq!(job.parent_span_id, None, "작업 span이 그 trace의 시작이다");
    assert_eq!(
        job.linked_trace_ids,
        vec![request_trace.clone()],
        "작업 span에서 접수 요청 trace로 link가 걸려야 한다"
    );
    assert_eq!(job.attribute("decision"), Some("transition"));
    assert!(!job.is_error);

    // 업체 호출: provider.call → provider.attempt → HTTP client span
    let spans = captured.spans();
    let call = span_in(&spans, &job_trace, "provider.call");
    assert_eq!(call.parent_span_id.as_deref(), Some(job.span_id.as_str()));
    assert_eq!(call.attribute("provider"), Some("alpha"));
    assert_eq!(call.attribute("operation"), Some("create_booking"));
    assert_eq!(
        call.attribute("booking_id"),
        Some(booking_id.to_string().as_str())
    );
    let attempt = span_in(&spans, &job_trace, "provider.attempt");
    assert_eq!(
        attempt.parent_span_id.as_deref(),
        Some(call.span_id.as_str())
    );
    assert_eq!(attempt.attribute("attempt"), Some("1"));
    assert_eq!(attempt.attribute("outcome"), Some("success"));
    let http_client = span_in(&spans, &job_trace, "POST /reservations");
    assert_eq!(http_client.kind, "client");
    assert_eq!(
        http_client.parent_span_id.as_deref(),
        Some(attempt.span_id.as_str())
    );
    assert_eq!(
        http_client.attribute("http.response.status_code"),
        Some("201")
    );

    // mock 업체 내부: traceparent 헤더로 같은 trace에 이어진다 (로드맵 Phase 4 완료 기준)
    let provider_side = wait_for_span(captured, |span| {
        span.trace_id == job_trace && span.name == "POST /alpha/v1/reservations"
    })
    .await;
    assert_eq!(provider_side.kind, "server");
    assert_eq!(
        provider_side.parent_span_id.as_deref(),
        Some(http_client.span_id.as_str()),
        "업체의 서버 span은 우리 client span 아래에 달려야 한다"
    );
    let spans = captured.spans();
    let delay = span_in(&spans, &job_trace, "chaos.delay");
    assert_eq!(
        delay.parent_span_id.as_deref(),
        Some(provider_side.span_id.as_str()),
        "업체 내부에서 기다린 시간이 trace에 보여야 한다"
    );
    for span in captured
        .spans()
        .iter()
        .filter(|span| span.trace_id == job_trace)
    {
        assert_no_customer_data(span);
    }

    // ── 3. DB에 남은 trace ID ──
    let booking = db::bookings::get(&pool, booking_id)
        .await
        .expect("예약을 읽을 수 있어야 한다")
        .expect("예약이 있어야 한다");
    assert_eq!(booking.status, BookingStatus::Confirmed);
    let event_traces: Vec<_> = db::bookings::events(&pool, booking_id)
        .await
        .expect("이력을 읽을 수 있어야 한다")
        .into_iter()
        .map(|event| event.trace_id)
        .collect();
    assert_eq!(
        event_traces,
        vec![Some(request_trace.clone()), Some(job_trace.clone())],
        "상태 이력에는 그 변경을 일으킨 trace가 남는다 (접수는 요청, 확정은 작업)"
    );
    let recorded = calls.try_recv().expect("호출 기록이 하나 있어야 한다");
    assert_eq!(recorded.outcome, "success");
    assert_eq!(recorded.trace_id, Some(job_trace.clone()));

    // ── 4. 오류 응답과 실려 온 traceparent ──
    let missing = send(&app, Method::GET, "/v1/bookings/not-a-uuid", &[auth], None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(
        missing.body["trace_id"].as_str(),
        trace_header(&missing).as_deref(),
        "오류 본문의 trace_id는 응답 헤더와 같다"
    );
    assert!(missing.body["trace_id"].is_string());

    let caller_trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let traceparent = format!("00-{caller_trace}-00f067aa0ba902b7-01");
    let found = send(
        &app,
        Method::GET,
        &format!("/v1/bookings/{booking_id}"),
        &[auth, ("traceparent", &traceparent)],
        None,
    )
    .await;
    assert_eq!(found.status, StatusCode::OK);
    assert_eq!(
        trace_header(&found).as_deref(),
        Some(caller_trace),
        "호출한 쪽이 trace를 실어 보내면 그 trace에 이어 붙는다"
    );

    let health = send(&app, Method::GET, "/healthz", &[], None).await;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(trace_header(&health), None, "헬스 체크는 계측하지 않는다");

    // ── 5. 지표: 정상 경로 ──
    let attempts = captured.metric("provider.call.duration");
    let success = point(
        &attempts,
        &[
            ("provider", "alpha"),
            ("operation", "create_booking"),
            ("outcome", "success"),
        ],
    );
    assert_eq!(success.value, 1.0);
    assert_eq!(
        success.bounds,
        telemetry::SECONDS_BUCKETS,
        "초 단위 버킷을 직접 지정해야 한다"
    );

    let requests = captured.metric("http.server.request.duration");
    for (route, method, status) in [
        ("/v1/bookings", "POST", "202"),
        ("/v1/bookings/{id}", "GET", "404"),
        ("/v1/bookings/{id}", "GET", "200"),
        // mock 업체도 같은 미들웨어로 계측한다
        ("/alpha/v1/reservations", "POST", "201"),
    ] {
        let found = point(
            &requests,
            &[
                ("http.route", route),
                ("http.request.method", method),
                ("http.response.status_code", status),
            ],
        );
        assert_eq!(found.value, 1.0, "{method} {route} {status}");
    }
    assert!(
        requests
            .iter()
            .all(|point| point.attribute("http.route") != Some("/healthz")),
        "헬스 체크는 지표에도 넣지 않는다"
    );

    let transitions = captured.metric("booking.transitions");
    for (from, to) in [("none", "pending"), ("pending", "confirmed")] {
        let found = point(
            &transitions,
            &[("provider", "alpha"), ("from", from), ("to", to)],
        );
        assert_eq!(found.value, 1.0, "{from} → {to}");
    }

    let long_buckets: Vec<f64> = telemetry::SECONDS_BUCKETS
        .into_iter()
        .chain(LONG_BUCKET_TAIL)
        .collect();
    let finals = captured.metric("booking.time_to_final");
    let confirmed = point(
        &finals,
        &[("provider", "alpha"), ("final_status", "confirmed")],
    );
    assert_eq!(confirmed.value, 1.0);
    assert_eq!(
        confirmed.bounds, long_buckets,
        "몇 분짜리 값도 구분되도록 긴 버킷을 쓴다"
    );
    let lag = captured.metric("jobs.lag");
    let lag = point(&lag, &[("kind", "submit_booking")]);
    assert_eq!(lag.value, 1.0);
    assert_eq!(lag.bounds, long_buckets);

    // 업체 상태 (지표를 읽을 때마다 현재 값을 가져온다)
    let alpha = [("provider", "alpha")];
    assert_eq!(
        point(&captured.metric("provider.circuit.state"), &alpha).value,
        0.0
    );
    assert_eq!(
        point(&captured.metric("provider.paused"), &alpha).value,
        0.0
    );
    assert_eq!(
        point(&captured.metric("provider.bulkhead.in_flight"), &alpha).value,
        0.0
    );
    // mock 업체가 응답 헤더(X-RateLimit-Remaining)로 알려 준 값
    assert!(point(&captured.metric("provider.rate_limit.remaining"), &alpha).value > 0.0);

    // ── 6. 지표: 큐 상태 (DB를 읽어 갱신한다) ──
    send(
        &app,
        Method::POST,
        "/v1/bookings",
        &[auth, ("idempotency-key", "order-2")],
        Some(&booking_body(&common::open_slot())),
    )
    .await;
    // 이 코드가 모르는 종류의 작업이 dead로 남아 있다 (다른 버전이 등록한 작업을 워커가 묻은 경우)
    sqlx::query("INSERT INTO jobs (kind, max_attempts, status) VALUES ('mystery_kind', 1, 'dead')")
        .execute(&pool)
        .await
        .expect("작업을 넣을 수 있어야 한다");
    metrics::refresh_queue_gauges(&pool)
        .await
        .expect("큐 상태를 읽을 수 있어야 한다");
    let ready = captured.metric("jobs.ready");
    assert_eq!(point(&ready, &[("kind", "submit_booking")]).value, 1.0);
    assert_eq!(
        point(&ready, &[("kind", "resolve_unknown")]).value,
        0.0,
        "해당 작업이 없는 종류도 0으로 내보낸다"
    );
    let dead = captured.metric("jobs.dead");
    assert_eq!(point(&dead, &[("kind", "submit_booking")]).value, 0.0);
    assert_eq!(
        point(&dead, &[("kind", "unknown")]).value,
        1.0,
        "모르는 종류의 dead 작업도 세어야 알림이 울린다"
    );
    assert_eq!(
        point(&captured.metric("booking.unknown.oldest_age"), &[]).value,
        0.0
    );

    // ── 7. 지표: 실패 경로 (5xx 뒤 재시도, 업체의 429, 그 뒤 pause) ──
    // counter는 업체 파이프라인을 만들 때 0으로 만들어 둔다. 그래야 Prometheus가 첫 증가부터 잡는다
    // (처음 나타난 값은 증가로 보지 않기 때문이다).
    let flaky_label = [("provider", "flaky")];
    for (metric, label) in [
        ("provider.call.retries", ("operation", "create_booking")),
        ("provider.call.retries", ("operation", "find_booking")),
        ("provider.throttled", ("source", "local")),
        ("provider.throttled", ("source", "remote")),
    ] {
        let points = captured.metric(metric);
        assert_eq!(
            point(&points, &[flaky_label[0], label]).value,
            0.0,
            "{metric} {label:?}"
        );
    }
    assert_eq!(
        point(&captured.metric("provider_calls.recorder.dropped"), &[]).value,
        0.0
    );

    let request = create_request();
    // 첫 호출: 503 → 같은 호출 안에서 재시도 → 성공
    flaky
        .create_booking(&request, CallContext::default())
        .await
        .expect("재시도로 성공해야 한다");
    // 둘째 호출: 업체가 429 + Retry-After 30초 → 업체 전체를 쉬게 한다
    let limited = flaky.create_booking(&request, CallContext::default()).await;
    assert!(matches!(limited, Err(ProviderError::RateLimited { .. })));
    // 셋째 호출: 쉬는 중이라 업체를 부르지 않는다
    let blocked = flaky.create_booking(&request, CallContext::default()).await;
    assert!(matches!(blocked, Err(ProviderError::Throttled { .. })));

    assert_eq!(
        point(
            &captured.metric("provider.call.retries"),
            &[("provider", "flaky"), ("operation", "create_booking")]
        )
        .value,
        1.0
    );
    let attempts = captured.metric("provider.call.duration");
    for (outcome, count) in [
        ("server_error", 1.0),
        ("success", 1.0),
        ("rate_limited", 1.0),
        // 부르지 않은 시도도 결과별 비율을 보기 위해 기록한다
        ("throttled", 1.0),
    ] {
        let found = point(&attempts, &[("provider", "flaky"), ("outcome", outcome)]);
        assert_eq!(found.value, count, "{outcome}");
    }
    let throttled = captured.metric("provider.throttled");
    for source in ["remote", "local"] {
        let found = point(&throttled, &[("provider", "flaky"), ("source", source)]);
        assert_eq!(found.value, 1.0, "{source}");
    }
    assert_eq!(
        point(&captured.metric("provider.paused"), &flaky_label).value,
        1.0,
        "Retry-After 동안은 쉬는 중으로 보여야 한다"
    );
    // 이 업체의 기록 채널은 1칸이고 아무도 읽지 않는다. 시도 4번 중 첫 기록만 들어가고 나머지는 버려진다.
    assert_eq!(
        point(&captured.metric("provider_calls.recorder.dropped"), &[]).value,
        3.0
    );

    // 실패한 시도는 trace에서 오류로 표시된다
    let failed_attempt = wait_for_span(captured, |span| {
        span.name == "provider.attempt" && span.attribute("outcome") == Some("server_error")
    })
    .await;
    assert!(failed_attempt.is_error);

    // ── 8. 업체가 오류 본문에 고객 정보를 되돌려 줘도 로그 · span · 실패 사유로 새지 않는다 ──
    let echoed = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &[auth, ("idempotency-key", "order-echo")],
        Some(&json!({
            "provider": "echo",
            "resource_id": "store-001",
            "slot_start": common::open_slot(),
            "party_size": 2,
            "customer": { "name": CUSTOMER_NAME, "phone": CUSTOMER_PHONE }
        })),
    )
    .await;
    assert_eq!(echoed.status, StatusCode::ACCEPTED, "{:?}", echoed.body);
    let echo_id = echoed.booking().id;
    // 앞에서 접수해 둔 다른 예약(order-2)의 작업이 먼저 나올 수 있으므로, 이 예약의 결론이 날 때까지 처리한다
    let failed = loop {
        let processed = worker::process_next(&ctx)
            .await
            .expect("작업을 처리할 수 있어야 한다");
        assert!(processed, "echo 예약의 작업이 처리되기 전에 큐가 비었다");
        let booking = db::bookings::get(&pool, echo_id)
            .await
            .expect("예약을 읽을 수 있어야 한다")
            .expect("예약이 있어야 한다");
        if booking.status != BookingStatus::Pending {
            break booking;
        }
    };
    assert_eq!(failed.status, BookingStatus::Failed);
    assert_eq!(
        failed.status_reason.as_deref(),
        Some("예상하지 못한 응답 (HTTP 422)"),
        "실패 사유에는 업체가 보낸 본문을 넣지 않는다"
    );

    let echo_job = wait_for_span(captured, |span| {
        span.name == "job.submit_booking"
            && span.attribute("booking_id") == Some(echo_id.to_string().as_str())
    })
    .await;
    assert_eq!(echo_job.attribute("decision"), Some("give_up"));
    assert!(echo_job.is_error, "포기한 작업은 오류 span이다");
    // 워커가 남긴 경고 로그는 span의 이벤트로도 기록된다. 거기에 오류 메시지는 있지만 본문은 없어야 한다.
    assert!(
        echo_job.events.iter().any(|event| {
            event.attributes.get("error").map(String::as_str)
                == Some("예상하지 못한 응답 (HTTP 422)")
        }),
        "{:?}",
        echo_job.events
    );
    for span in captured
        .spans()
        .iter()
        .filter(|span| span.trace_id == echo_job.trace_id)
    {
        assert_no_customer_data(span);
    }
    // 원인을 찾을 수 있도록 DB의 호출 기록에는 본문이 남는다
    let echo_call = echo_calls.try_recv().expect("호출 기록이 하나 있어야 한다");
    assert_eq!(echo_call.outcome, "client_error");
    assert!(
        echo_call
            .error_detail
            .is_some_and(|detail| detail.contains(CUSTOMER_NAME)),
        "호출 기록에만 업체가 보낸 본문을 남긴다"
    );

    // ── 9. 응답을 받기 전에 버려진 요청도 기록한다 ──
    // 업체가 응답하지 않는다. 호출 예산(150ms)이 시도 타임아웃(2초)보다 짧아서, HTTP 클라이언트 자신의 타임아웃이
    // 아니라 바깥의 타임아웃이 기다리던 요청을 **버린다**. 버려진 쪽은 뒤의 코드를 실행하지 못한다.
    mock.chaos.set(
        mock_provider::Provider::Alpha,
        ChaosConfig {
            hang_probability: 1.0,
            ..ChaosConfig::passthrough()
        },
    );
    let impatient_policy = ProviderPolicy {
        attempt_timeout_ms: 2_000,
        call_budget_ms: 150,
        ..single_attempt_policy()
    };
    let impatient = ResilientProvider::new(
        ProviderId::parse("impatient").expect("올바른 업체 ID"),
        providers::build_adapter("alpha_v1", &mock_url, &impatient_policy)
            .expect("어댑터를 만들 수 있어야 한다"),
        impatient_policy,
        CallRecorder::discard(),
    );
    let probe = tracing::info_span!("hang-probe");
    let hang_trace = telemetry::trace_id_of(&probe).expect("span에는 trace ID가 있다");
    let hung = impatient
        .create_booking(&create_request(), CallContext::default())
        .instrument(probe)
        .await;
    assert!(matches!(hung, Err(ProviderError::Timeout)), "{hung:?}");

    // 우리 쪽: HTTP client span이 오류로 끝난다
    let abandoned = wait_for_span(captured, |span| {
        span.trace_id == hang_trace && span.name == "POST /reservations"
    })
    .await;
    assert!(abandoned.is_error, "버려진 요청은 오류 span이다");
    assert_eq!(abandoned.attribute("error.type"), Some("cancelled"));
    assert_eq!(abandoned.attribute("http.response.status_code"), None);

    // 업체 쪽: 호출한 쪽이 끊어서 응답을 보내지 못한 요청도 span과 지연 지표에 남는다.
    // 이것이 빠지면 "너무 느려서 상대가 포기한 요청"이 통계에서 사라져 실제보다 빨라 보인다.
    let unanswered = wait_for_span(captured, |span| {
        span.trace_id == hang_trace && span.name == "POST /alpha/v1/reservations"
    })
    .await;
    assert!(unanswered.is_error);
    assert_eq!(unanswered.attribute("error.type"), Some("no_response"));
    assert_eq!(unanswered.attribute("http.response.status_code"), None);
    let requests = captured.metric("http.server.request.duration");
    let no_response = point(
        &requests,
        &[
            ("http.route", "/alpha/v1/reservations"),
            ("error.type", "no_response"),
        ],
    );
    assert_eq!(no_response.value, 1.0);
    assert_eq!(no_response.attribute("http.response.status_code"), None);
}

// ───────────────────────── 준비 코드 ─────────────────────────

/// 테스트 예약의 고객 정보 (`common::booking_body`와 같은 값). 이 글자가 span이나 로그에 나오면 안 된다.
const CUSTOMER_NAME: &str = "홍길동";
const CUSTOMER_PHONE: &str = "010-0000-0000";

/// mock 업체를 임의 포트에 띄우고, DB의 alpha 주소를 그쪽으로 바꾼다. mock의 상태와 alpha API 주소를 돌려준다.
/// 요청마다 10~20ms를 기다리게 해서 "업체 내부의 지연"이 trace에 나타나게 한다.
async fn start_mock(pool: &PgPool) -> (mock_provider::AppState, String) {
    let chaos = ChaosConfig {
        latency_ms: LatencyRange { min: 10, max: 20 },
        ..ChaosConfig::passthrough()
    };
    let state = mock_provider::AppState::new(&chaos);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("임의 포트에 바인드할 수 있어야 한다");
    let addr = listener.local_addr().expect("주소를 알 수 있어야 한다");
    let app = mock_provider::app(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });

    let url = format!("http://{addr}/alpha/v1");
    sqlx::query("UPDATE providers SET base_url = $1, policy = $2 WHERE id = 'alpha'")
        .bind(&url)
        .bind(serde_json::to_value(single_attempt_policy()).expect("정책은 JSON으로 바뀐다"))
        .execute(pool)
        .await
        .expect("업체 설정을 바꿀 수 있어야 한다");
    (state, url)
}

/// 업체 `echo`를 DB에 등록하고, 그 자리에 넣을 가짜 업체를 돌려준다.
/// 이 업체는 예약 생성에 422로 답하면서 오류 본문에 요청의 고객 정보를 그대로 되돌려 준다.
async fn echo_provider(pool: &PgPool) -> (Arc<ResilientProvider>, mpsc::Receiver<CallRecord>) {
    sqlx::query(
        "INSERT INTO providers (id, display_name, adapter_kind, base_url)
         VALUES ('echo', 'Echo (test)', 'alpha_v1', 'http://127.0.0.1:1/unused')",
    )
    .execute(pool)
    .await
    .expect("업체를 등록할 수 있어야 한다");

    let body = format!(
        r#"{{"error":{{"code":"invalid","message":"customer {CUSTOMER_NAME} / {CUSTOMER_PHONE} is not acceptable"}}}}"#
    );
    let adapter = FakeAdapter::new(
        IDEMPOTENT,
        vec![Err(ProviderError::Client { status: 422, body })],
    );
    let (recorder, receiver) = providers::call_recorder(4);
    let provider = ResilientProvider::new(
        ProviderId::parse("echo").expect("올바른 업체 ID"),
        Arc::new(adapter),
        single_attempt_policy(),
        recorder,
    );
    (Arc::new(provider), receiver)
}

/// 정해 둔 순서로 실패하는 업체: 503 → 성공 → 429(Retry-After 30초).
/// 호출 내 재시도를 한 번 허용하고, 호출 기록 채널은 1칸뿐이다. 받는 쪽도 함께 돌려준다
/// (받는 쪽이 사라지면 채널이 닫혀서 "가득 차서 버림"이 아니라 "닫혀서 버림"이 된다).
fn flaky_provider() -> (Arc<ResilientProvider>, mpsc::Receiver<CallRecord>) {
    let adapter = FakeAdapter::new(
        IDEMPOTENT,
        vec![
            Err(ProviderError::Server { status: 503 }),
            Ok(ProviderBooking {
                provider_booking_id: "rsv_1".into(),
            }),
            Err(ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(30)),
            }),
        ],
    );
    let policy = ProviderPolicy {
        retry: RetryPolicy {
            max_attempts: 2,
            base_delay_ms: 1,
            max_delay_ms: 2,
        },
        ..single_attempt_policy()
    };
    let (recorder, receiver) = providers::call_recorder(1);
    let provider = ResilientProvider::new(
        ProviderId::parse("flaky").expect("올바른 업체 ID"),
        Arc::new(adapter),
        policy,
        recorder,
    );
    (Arc::new(provider), receiver)
}

fn create_request() -> CreateBooking {
    CreateBooking {
        reference: BookingId::from_uuid(Uuid::now_v7()),
        resource_id: "store-001".into(),
        slot_start: Utc::now(),
        party_size: 2,
        customer: Customer {
            name: "홍길동".into(),
            phone: "010-0000-0000".into(),
        },
    }
}

fn trace_header(reply: &Reply) -> Option<String> {
    reply
        .headers
        .get(telemetry::TRACE_ID_HEADER)
        .map(|value| value.to_str().expect("ASCII 헤더").to_owned())
}

/// `trace_id` trace 안에서 이름이 `name`인 span
fn span_in<'a>(spans: &'a [SpanView], trace_id: &str, name: &str) -> &'a SpanView {
    spans
        .iter()
        .find(|span| span.trace_id == trace_id && span.name == name)
        .unwrap_or_else(|| {
            let names: Vec<_> = spans
                .iter()
                .filter(|span| span.trace_id == trace_id)
                .map(|span| span.name.as_str())
                .collect();
            panic!("trace {trace_id}에 span {name:?}이 없다. 있는 span: {names:?}")
        })
}

/// 조건에 맞는 span이 수집될 때까지 잠깐 기다린다.
/// (mock 업체의 span은 응답을 보낸 뒤 다른 태스크에서 끝나므로, 우리 쪽 호출이 끝난 직후에는 아직 없을 수 있다)
async fn wait_for_span(captured: &Captured, found: impl Fn(&SpanView) -> bool) -> SpanView {
    for _ in 0..200 {
        if let Some(span) = captured.spans().into_iter().find(&found) {
            return span;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("기다린 span이 수집되지 않았다");
}

/// 라벨이 모두 일치하는 시계열 하나
fn point<'a>(points: &'a [MetricPoint], labels: &[(&str, &str)]) -> &'a MetricPoint {
    points
        .iter()
        .find(|point| {
            labels
                .iter()
                .all(|(key, value)| point.attribute(key) == Some(*value))
        })
        .unwrap_or_else(|| panic!("라벨 {labels:?}인 시계열이 없다: {points:#?}"))
}

/// 고객 이름 · 전화번호가 span 속성이나 span 안에서 남긴 로그에 들어가면 안 된다 (CLAUDE.md 코딩 규칙)
fn assert_no_customer_data(span: &SpanView) {
    let leaked = |text: &str| text.contains(CUSTOMER_NAME) || text.contains(CUSTOMER_PHONE);
    for (key, value) in &span.attributes {
        assert!(
            !leaked(value),
            "span {}의 속성 {key}에 고객 정보가 들어 있다: {value}",
            span.name
        );
    }
    for event in &span.events {
        assert!(
            !leaked(&event.name) && !event.attributes.values().any(|value| leaked(value)),
            "span {}의 로그에 고객 정보가 들어 있다: {event:?}",
            span.name
        );
    }
}
