//! HTTP 계측 미들웨어와 trace 전파 도구 테스트
//!
//! 수집기 없이 메모리에 모은 span · 메트릭으로 확인한다 (`telemetry::testing`).
//! 전역 subscriber는 프로세스에 하나뿐이라 테스트들이 같은 수집 결과를 함께 쓴다. 그래서 테스트마다 자기 trace ID나
//! 자기만 쓰는 경로로 결과를 골라낸다.

use std::time::Duration;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::get,
};
use telemetry::{
    HttpTelemetryLayer, TRACE_ID_HEADER, TraceCarrier,
    testing::{Captured, SpanView},
};
use tower::ServiceExt;
use tracing::Instrument;

/// 테스트용 로그 필터: 모든 span을 본다
const FILTER: &str = "debug";

fn app() -> Router {
    Router::new()
        // 핸들러 안에서 본 trace ID를 본문으로 돌려준다
        .route(
            "/items/{id}",
            get(|| async { telemetry::current_trace_id().unwrap_or_default() }),
        )
        .route(
            "/broken",
            get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        )
        // 응답이 아주 늦는 핸들러 (호출한 쪽이 먼저 포기하는 상황을 만든다)
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "late"
            }),
        )
        .layer(HttpTelemetryLayer::new())
        // layer보다 뒤에 등록한 라우트에는 계측이 걸리지 않는다
        .route("/healthz", get(|| async { "ok" }))
}

struct Reply {
    status: StatusCode,
    trace_id: Option<String>,
    body: String,
}

async fn send(app: &Router, path: &str, traceparent: Option<&str>) -> Reply {
    let mut request = Request::builder().uri(path);
    if let Some(traceparent) = traceparent {
        request = request.header("traceparent", traceparent);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).expect("올바른 요청"))
        .await
        .expect("axum Router는 실패하지 않는다");
    let status = response.status();
    let trace_id = response
        .headers()
        .get(TRACE_ID_HEADER)
        .map(|value| value.to_str().expect("ASCII 헤더").to_owned());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("본문을 읽을 수 있어야 한다");
    Reply {
        status,
        trace_id,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// 조건에 맞는 span이 나올 때까지 잠깐 기다린다 (span은 끝나는 순간 수집된다)
async fn wait_for_span(captured: &Captured, found: impl Fn(&SpanView) -> bool) -> SpanView {
    for _ in 0..100 {
        if let Some(span) = captured.spans().into_iter().find(&found) {
            return span;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("기다린 span이 수집되지 않았다");
}

#[tokio::test]
async fn 요청마다_서버_span을_열고_응답에_trace_id를_붙인다() {
    let captured = telemetry::testing::install(FILTER);
    let app = app();

    let reply = send(&app, "/items/42", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    let trace_id = reply
        .trace_id
        .expect("응답에 X-Trace-Id 헤더가 있어야 한다");
    assert_eq!(trace_id.len(), 32, "trace ID는 16진수 32자다");
    assert_eq!(
        reply.body, trace_id,
        "핸들러 안에서 보는 trace가 응답 헤더의 trace와 같아야 한다"
    );

    let span = wait_for_span(captured, |span| span.trace_id == trace_id).await;
    assert_eq!(span.name, "GET /items/{id}", "이름에는 라우트 틀을 쓴다");
    assert_eq!(span.kind, "server");
    assert_eq!(
        span.parent_span_id, None,
        "traceparent가 없으면 새 trace의 시작이다"
    );
    assert_eq!(span.attribute("http.route"), Some("/items/{id}"));
    assert_eq!(span.attribute("url.path"), Some("/items/42"));
    assert_eq!(span.attribute("http.response.status_code"), Some("200"));
    assert!(!span.is_error);
}

#[tokio::test]
async fn traceparent_헤더가_있으면_그_trace에_이어_붙는다() {
    let captured = telemetry::testing::install(FILTER);
    let app = app();
    let (caller_trace, caller_span) = ("4bf92f3577b34da6a3ce929d0e0e4736", "00f067aa0ba902b7");

    let reply = send(
        &app,
        "/items/7",
        Some(&format!("00-{caller_trace}-{caller_span}-01")),
    )
    .await;
    assert_eq!(reply.trace_id.as_deref(), Some(caller_trace));

    let span = wait_for_span(captured, |span| span.trace_id == caller_trace).await;
    assert_eq!(
        span.parent_span_id.as_deref(),
        Some(caller_span),
        "부른 쪽의 span이 부모가 된다"
    );

    // 형식이 틀린 헤더는 무시하고 새 trace를 시작한다
    let reply = send(&app, "/items/8", Some("not-a-traceparent")).await;
    let fresh = reply.trace_id.expect("새 trace ID");
    assert_ne!(fresh, caller_trace);
}

#[tokio::test]
async fn 서버_오류는_span에_오류로_표시하고_지표에_상태_코드를_남긴다() {
    let captured = telemetry::testing::install(FILTER);
    let app = app();

    let reply = send(&app, "/broken", None).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    let trace_id = reply.trace_id.expect("오류 응답에도 trace ID가 있다");
    let span = wait_for_span(captured, |span| span.trace_id == trace_id).await;
    assert!(span.is_error, "5xx는 오류 span이다");

    let points = captured.metric("http.server.request.duration");
    let broken = points
        .iter()
        .find(|point| point.attribute("http.route") == Some("/broken"))
        .expect("요청 시간 지표가 기록되어야 한다");
    assert_eq!(broken.attribute("http.request.method"), Some("GET"));
    assert_eq!(broken.attribute("http.response.status_code"), Some("500"));
    assert!(broken.value >= 1.0);
    assert_eq!(
        broken.bounds,
        telemetry::SECONDS_BUCKETS,
        "초 단위 버킷을 직접 지정해야 한다 (기본 버킷은 밀리초 기준)"
    );
}

#[tokio::test]
async fn 응답을_보내기_전에_버려진_요청도_span과_지표에_남는다() {
    let captured = telemetry::testing::install(FILTER);
    let app = app();

    // 호출한 쪽이 50ms만 기다리고 포기한다. 서버는 처리하던 Future를 그 자리에서 버린다.
    let gave_up = tokio::time::timeout(Duration::from_millis(50), send(&app, "/slow", None)).await;
    assert!(gave_up.is_err(), "응답이 오기 전에 포기해야 한다");

    let span = wait_for_span(captured, |span| span.attribute("url.path") == Some("/slow")).await;
    assert!(span.is_error);
    assert_eq!(span.attribute("error.type"), Some("no_response"));
    assert_eq!(span.attribute("http.response.status_code"), None);

    let points = captured.metric("http.server.request.duration");
    let abandoned = points
        .iter()
        .find(|point| point.attribute("http.route") == Some("/slow"))
        .expect("응답하지 못한 요청도 걸린 시간을 기록해야 한다");
    assert_eq!(abandoned.attribute("error.type"), Some("no_response"));
    assert_eq!(abandoned.attribute("http.response.status_code"), None);
    assert_eq!(abandoned.value, 1.0);
}

#[tokio::test]
async fn 없는_경로는_라우트_라벨_없이_기록하고_layer_뒤의_라우트는_계측하지_않는다() {
    let captured = telemetry::testing::install(FILTER);
    let app = app();

    // 없는 경로: 404도 계측한다. 다만 경로를 그대로 이름 · 라벨로 쓰지 않는다 (값이 끝없이 늘어날 수 있다).
    let reply = send(&app, "/no/such/path", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let trace_id = reply.trace_id.expect("404에도 trace ID가 있다");
    let span = wait_for_span(captured, |span| span.trace_id == trace_id).await;
    assert_eq!(span.name, "GET");
    assert_eq!(span.attribute("http.route"), None);
    let unmatched = captured
        .metric("http.server.request.duration")
        .into_iter()
        .filter(|point| point.attribute("http.response.status_code") == Some("404"))
        .collect::<Vec<_>>();
    assert!(!unmatched.is_empty());
    assert!(
        unmatched
            .iter()
            .all(|point| point.attribute("http.route").is_none())
    );

    // layer 뒤에 등록한 라우트
    let reply = send(&app, "/healthz", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.trace_id, None, "계측하지 않는 라우트");
}

#[tokio::test]
async fn 실어_보낸_trace_정보로_다른_trace에서_link를_건다() {
    let captured = telemetry::testing::install(FILTER);

    // 접수 요청 쪽: 지금 span의 trace 정보를 꺼내 둔다 (작업 큐에 저장하는 것에 해당)
    let origin = tracing::info_span!("origin");
    let (carrier, origin_trace) = async {
        (
            telemetry::current_carrier(),
            telemetry::current_trace_id().expect("span 안에서는 trace ID가 있다"),
        )
    }
    .instrument(origin)
    .await;
    assert!(
        carrier
            .get("traceparent")
            .is_some_and(|value| value.contains(&origin_trace)),
        "{carrier:?}"
    );

    // 작업 처리 쪽: 새 trace에서 시작하고, 접수 요청으로 가는 link만 건다
    let job = tracing::info_span!("job-with-origin");
    telemetry::link_to_origin(&job, &carrier);
    let job_trace = telemetry::trace_id_of(&job).expect("trace ID");
    drop(job);
    assert_ne!(job_trace, origin_trace, "같은 trace로 잇지 않는다");
    let span = wait_for_span(captured, |span| span.name == "job-with-origin").await;
    assert_eq!(span.linked_trace_ids, vec![origin_trace]);

    // 실려 온 정보가 없으면 link를 걸지 않는다 (자기 자신에게 link를 걸면 안 된다)
    let orphan = tracing::info_span!("job-without-origin");
    async { telemetry::link_to_origin(&tracing::Span::current(), &TraceCarrier::new()) }
        .instrument(orphan)
        .await;
    let span = wait_for_span(captured, |span| span.name == "job-without-origin").await;
    assert_eq!(span.linked_trace_ids, Vec::<String>::new());
}

#[tokio::test]
async fn span_밖에서는_trace_정보가_없다() {
    telemetry::testing::install(FILTER);
    assert_eq!(telemetry::current_trace_id(), None);
    assert!(telemetry::current_carrier().is_empty());
}
