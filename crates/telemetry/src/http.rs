//! HTTP 서버 계측 미들웨어 (docs/05-observability-and-analytics.md §3, §4)
//!
//! 요청마다 다음을 한다.
//! 1. 요청 헤더 `traceparent`에 실려 온 trace가 있으면 그 뒤에 잇고, 없으면 새 trace를 시작한다
//! 2. 서버 span을 열고 그 안에서 핸들러를 실행한다 (핸들러가 남기는 로그 · 하위 span이 모두 이 span 아래에 달린다)
//! 3. 응답이 나오면 상태 코드를 span에 남기고, 걸린 시간을 `http.server.request.duration` 지표에 기록하고,
//!    응답 헤더 `X-Trace-Id`에 trace ID를 넣는다 (docs/04-api.md §1)
//! 4. 응답을 보내기 전에 요청이 끝나 버려도(호출한 쪽이 먼저 연결을 끊은 경우 등) 걸린 시간은 기록한다.
//!    이것을 빼면 "너무 느려서 상대가 포기한 요청"이 지연 통계에서 사라져 실제보다 빨라 보인다.
//!
//! ## tower의 `Service`와 `Layer`
//! - [`Service`]: "요청을 받아 응답을 돌려주는 비동기 함수"를 나타내는 trait. axum의 핸들러와 라우터가 모두 `Service`다.
//! - [`Layer`]: `Service`를 받아 **한 겹 감싼** `Service`를 돌려주는 공장
//!   (≈ ASP.NET Core의 미들웨어 클래스. `InvokeAsync(context, next)`에서 `next`가 안쪽 `Service`에 해당한다).
//!
//! mock-provider의 장애 주입 미들웨어는 `axum::middleware::from_fn`으로 만들었다. 그 함수가 대신 해 주던 일
//! (`Layer` · `Service` 구현)을 여기서는 직접 쓴다. 이렇게 만든 미들웨어는 axum뿐 아니라 tower를 쓰는 곳 어디에나 끼울 수 있다.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use axum::{
    extract::MatchedPath,
    http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode},
};
use opentelemetry::{
    KeyValue, global,
    metrics::{Histogram, Meter},
};
use tower::{Layer, Service};
use tracing::{Instrument, Span, field::Empty};

use crate::{SECONDS_BUCKETS, propagation};

/// 응답에 trace ID를 실어 주는 헤더. 문제를 신고받았을 때 이 값으로 Grafana에서 그 요청의 trace를 찾는다.
pub const TRACE_ID_HEADER: HeaderName = HeaderName::from_static("x-trace-id");

/// 힙에 올린 Future (≈ C#의 `Task<T>`).
///
/// `async` 블록은 블록마다 이름 없는 고유한 타입이 된다. trait의 연관 타입(`Service::Future`)에는 타입 이름을 적어야 해서,
/// `Box<dyn Future>`로 감싸 "어떤 Future든 담는 하나의 타입"으로 만든다. `Pin`은 Future가 메모리에서 옮겨지지 않게 고정한다
/// (`.await` 지점 사이에 자기 내부를 가리키는 참조가 있을 수 있어서, 실행하려면 고정되어 있어야 한다).
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 서버 요청을 계측하는 [`Layer`]. `Router::layer(HttpTelemetryLayer::new())`로 건다.
#[derive(Debug, Clone)]
pub struct HttpTelemetryLayer {
    duration: Histogram<f64>,
}

impl HttpTelemetryLayer {
    /// 전역 meter provider에 지표를 등록한다. `telemetry::init` 뒤에 불러야 지표가 내보내진다.
    pub fn new() -> Self {
        Self::with_meter(&global::meter("telemetry"))
    }

    /// 지표를 등록할 meter를 직접 고른다 (테스트용)
    pub fn with_meter(meter: &Meter) -> Self {
        let duration = meter
            .f64_histogram("http.server.request.duration")
            .with_unit("s")
            .with_description("HTTP 요청을 받아 응답을 돌려주기까지 걸린 시간")
            .with_boundaries(SECONDS_BUCKETS.to_vec())
            .build();
        Self { duration }
    }
}

impl Default for HttpTelemetryLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Layer<S> for HttpTelemetryLayer {
    type Service = HttpTelemetry<S>;

    fn layer(&self, inner: S) -> Self::Service {
        HttpTelemetry {
            inner,
            duration: self.duration.clone(),
        }
    }
}

/// [`HttpTelemetryLayer`]가 만드는 `Service`. 안쪽 서비스(`inner`)를 감싼다.
#[derive(Debug, Clone)]
pub struct HttpTelemetry<S> {
    inner: S,
    duration: Histogram<f64>,
}

/// `S`는 안쪽 서비스의 타입이다. 요청 · 응답 본문 타입(`ReqBody`, `ResBody`)은 무엇이든 받도록 제네릭으로 둔다.
impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for HttpTelemetry<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    /// "지금 요청을 받을 수 있는가"를 안쪽 서비스에 그대로 묻는다 (배압을 전달하는 통로)
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let method = method_label(request.method());
        // axum이 찾아 둔 라우트 틀 (`/v1/bookings/{id}`). 실제 경로(`/v1/bookings/0193…`)를 지표 라벨로 쓰면
        // 값이 끝없이 늘어나므로 반드시 틀을 쓴다. 맞는 라우트가 없으면(404) 라벨을 붙이지 않는다.
        let route = request
            .extensions()
            .get::<MatchedPath>()
            .map(|matched| matched.as_str().to_owned());

        let span = request_span(method, route.as_deref(), request.uri().path());
        propagation::set_remote_parent(&span, request.headers());
        let trace_id = propagation::trace_id_of(&span);
        if let Some(trace_id) = &trace_id {
            span.record("trace_id", trace_id.as_str());
        }

        let record = RequestRecord {
            duration: self.duration.clone(),
            method,
            route,
            started: Instant::now(),
            span: span.clone(),
            finished: false,
        };
        // tower의 약속: `poll_ready`가 준비됐다고 한 바로 그 서비스로 요청을 보내야 한다.
        // Future 안으로 옮겨 갈(move) 것이 필요하므로, 준비된 `self.inner`를 꺼내 가고 그 자리에는 복제본을 넣어 둔다.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        let future = async move {
            let mut response = inner.call(request).await?;
            record.finish(response.status());
            if let Some(value) = trace_id.and_then(|id| HeaderValue::from_str(&id).ok()) {
                response.headers_mut().insert(TRACE_ID_HEADER, value);
            }
            Ok(response)
        };
        // `.instrument(span)`: 이 Future가 실행되는 동안(await로 쉬었다가 다시 깨어날 때마다) span 안에 있게 한다.
        // async 코드에서는 `span.enter()` 대신 이것을 쓴다. enter한 채로 await하면 다른 태스크의 로그가 이 span에 섞인다.
        Box::pin(future.instrument(span))
    }
}

/// 요청 하나의 결과를 **정확히 한 번** 기록한다.
///
/// 응답이 나오면 [`RequestRecord::finish`]로 기록한다. 그런데 응답이 나오기 전에 요청이 끝나는 경우가 있다:
/// 호출한 쪽이 기다리다 연결을 끊으면 서버는 처리하던 Future를 **그 자리에서 버린다** (뒤의 코드는 실행되지 않는다).
/// 그때 할 일은 `Drop`에 적어 둔다. 값이 사라질 때는 반드시 불리기 때문이다 (≈ C#의 `using` + `Dispose`).
struct RequestRecord {
    duration: Histogram<f64>,
    method: &'static str,
    route: Option<String>,
    started: Instant,
    span: Span,
    finished: bool,
}

impl RequestRecord {
    /// 응답이 나왔다
    fn finish(mut self, status: StatusCode) {
        self.finished = true;
        let elapsed = self.started.elapsed();
        self.span
            .record("http.response.status_code", status.as_u16());
        if status.is_server_error() {
            // 5xx만 오류로 표시한다. 4xx는 클라이언트 쪽 문제라 서버 span으로서는 정상 처리다.
            self.span.record("otel.status_code", "error");
        }
        self.duration.record(
            elapsed.as_secs_f64(),
            &metric_attributes(self.method, self.route.as_deref(), Outcome::Status(status)),
        );
        tracing::debug!(
            status = status.as_u16(),
            latency_ms = elapsed.as_millis() as u64,
            "request finished"
        );
    }
}

impl Drop for RequestRecord {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // 응답을 보내지 못하고 끝났다: 호출한 쪽이 먼저 끊었거나, 핸들러가 패닉했다
        let elapsed = self.started.elapsed();
        self.span.record("otel.status_code", "error");
        self.span.record("error.type", NO_RESPONSE);
        self.duration.record(
            elapsed.as_secs_f64(),
            &metric_attributes(self.method, self.route.as_deref(), Outcome::NoResponse),
        );
        tracing::debug!(
            latency_ms = elapsed.as_millis() as u64,
            "request ended without a response"
        );
    }
}

/// 응답 없이 끝난 요청에 붙이는 `error.type` 값
const NO_RESPONSE: &str = "no_response";

/// 요청이 어떻게 끝났는가 (지표 라벨을 고르는 데 쓴다)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// 이 상태 코드로 응답했다
    Status(StatusCode),
    /// 응답을 보내지 못했다
    NoResponse,
}

/// 서버 span. 이름과 속성은 OpenTelemetry의 HTTP 규약을 따른다 (Grafana가 이 이름들을 알아본다).
///
/// `otel.`로 시작하는 필드는 tracing-opentelemetry가 특별하게 다룬다: `otel.name`은 span 이름(틀이 아니라 실제 값을 쓸 수 있다),
/// `otel.kind`는 종류(서버가 받은 요청), `otel.status_code`는 성공 · 실패 표시다.
/// 값을 나중에 채울 필드는 `Empty`로 자리만 잡아 둔다 (선언하지 않은 필드는 나중에 기록할 수 없다).
fn request_span(method: &'static str, route: Option<&str>, path: &str) -> Span {
    let name = match route {
        Some(route) => format!("{method} {route}"),
        None => method.to_owned(),
    };
    tracing::info_span!(
        "http.request",
        otel.name = %name,
        otel.kind = "server",
        otel.status_code = Empty,
        http.request.method = method,
        http.route = route,
        url.path = path,
        http.response.status_code = Empty,
        error.type = Empty,
        trace_id = Empty,
    )
}

/// 지표 라벨. 응답했으면 상태 코드를, 응답하지 못했으면 `error.type`을 붙인다 (OpenTelemetry의 HTTP 규약).
fn metric_attributes(method: &'static str, route: Option<&str>, outcome: Outcome) -> Vec<KeyValue> {
    let mut attributes = vec![KeyValue::new("http.request.method", method)];
    match outcome {
        Outcome::Status(status) => attributes.push(KeyValue::new(
            "http.response.status_code",
            i64::from(status.as_u16()),
        )),
        Outcome::NoResponse => attributes.push(KeyValue::new("error.type", NO_RESPONSE)),
    }
    if let Some(route) = route {
        attributes.push(KeyValue::new("http.route", route.to_owned()));
    }
    attributes
}

/// 지표 라벨로 쓸 메서드 이름. 표준 메서드가 아니면 `_OTHER`로 묶는다
/// (아무 문자열이나 메서드로 보낼 수 있어서, 그대로 쓰면 라벨 값이 끝없이 늘어날 수 있다).
fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::PATCH => "PATCH",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "_OTHER",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 표준이_아닌_메서드는_하나로_묶는다() {
        assert_eq!(method_label(&Method::POST), "POST");
        let custom = Method::from_bytes(b"PURGE").unwrap();
        assert_eq!(method_label(&custom), "_OTHER");
    }

    #[test]
    fn 라우트가_없으면_지표_라벨에서_뺀다() {
        let matched = metric_attributes(
            "GET",
            Some("/v1/bookings/{id}"),
            Outcome::Status(StatusCode::NOT_FOUND),
        );
        assert_eq!(matched.len(), 3);
        assert!(
            matched
                .iter()
                .any(|kv| kv.key.as_str() == "http.route"
                    && kv.value.as_str() == "/v1/bookings/{id}")
        );

        let unmatched = metric_attributes("GET", None, Outcome::Status(StatusCode::NOT_FOUND));
        assert!(unmatched.iter().all(|kv| kv.key.as_str() != "http.route"));
    }

    #[test]
    fn 응답하지_못한_요청은_상태_코드_대신_error_type을_붙인다() {
        let attributes = metric_attributes("POST", Some("/items"), Outcome::NoResponse);
        assert!(
            attributes
                .iter()
                .any(|kv| kv.key.as_str() == "error.type" && kv.value.as_str() == "no_response")
        );
        assert!(
            attributes
                .iter()
                .all(|kv| kv.key.as_str() != "http.response.status_code")
        );
    }
}
