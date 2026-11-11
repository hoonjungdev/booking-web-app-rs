//! 업체 호출을 trace로 잇기 (docs/05-observability-and-analytics.md §3)
//!
//! ```text
//! provider.attempt
//! └─ POST /reservations                 [client span: 여기서 요청 헤더에 traceparent를 넣는다]
//!    └─ POST /alpha/v1/reservations     [server span: mock-provider가 traceparent를 읽어 같은 trace로 잇는다]
//! ```
//!
//! 여기서는 OpenTelemetry의 "지금 Context"를 읽는다. tracing span 안에 들어가 있으면 그 span이 지금 Context가 된다
//! (실행 파일이 telemetry 크레이트로 설치한 layer가 해 주는 일). 설치하지 않았으면(테스트 등) trace가 없어서
//! 헤더를 넣지 않고 trace ID도 `None`이다.

use opentelemetry::{Context, global, trace::TraceContextExt};
use opentelemetry_http::HeaderInjector;
use reqwest::{RequestBuilder, Response};
use tracing::{Instrument, Span, field::Empty};

/// 지금 span의 trace ID (16진수 32자). trace가 없으면 `None`.
pub(crate) fn current_trace_id() -> Option<String> {
    let context = Context::current();
    let span = context.span();
    let span_context = span.span_context();
    span_context
        .is_valid()
        .then(|| span_context.trace_id().to_string())
}

/// 업체에 요청을 보내고 응답을 받는다. HTTP 클라이언트 span을 열고, 요청 헤더에 `traceparent`를 넣는다.
///
/// `route`는 span 이름에 쓸 경로 틀이다 (예: `/reservations`). 실제 경로에는 예약 ID 같은 값이 들어갈 수 있어서
/// 이름으로 쓰지 않는다 (span 이름의 종류가 끝없이 늘어나면 Grafana에서 묶어 보기 어렵다).
pub(crate) async fn send(
    builder: RequestBuilder,
    route: &'static str,
) -> Result<Response, reqwest::Error> {
    // `build_split`: 빌더를 "보낼 클라이언트"와 "완성된 요청"으로 나눈다. 요청을 보내기 전에 헤더를 더 넣기 위해서다.
    let (client, request) = builder.build_split();
    let mut request = request?;

    let span = tracing::info_span!(
        "http.client",
        otel.name = %format!("{} {route}", request.method()),
        otel.kind = "client",
        otel.status_code = Empty,
        http.request.method = %request.method(),
        server.address = request.url().host_str(),
        server.port = request.url().port_or_known_default(),
        url.path = request.url().path(),
        http.response.status_code = Empty,
        error.type = Empty,
    );
    async move {
        let mut pending = Pending {
            span: Span::current(),
            finished: false,
        };
        // 이 블록은 client span 안에서 실행되므로, 헤더에 들어가는 것은 이 span의 ID다.
        // 받는 쪽은 그 ID를 부모로 삼아 자기 span을 이 span 아래에 단다.
        global::get_text_map_propagator(|propagator| {
            propagator.inject(&mut HeaderInjector(request.headers_mut()));
        });

        let result = client.execute(request).await;
        pending.finished = true;
        let span = Span::current();
        match &result {
            Ok(response) => {
                span.record("http.response.status_code", response.status().as_u16());
                // 클라이언트 span은 4xx도 오류로 표시한다 (OpenTelemetry HTTP 규약)
                if response.status().is_client_error() || response.status().is_server_error() {
                    span.record("otel.status_code", "error");
                }
            }
            Err(_) => {
                span.record("otel.status_code", "error");
            }
        }
        result
    }
    .instrument(span)
    .await
}

/// 응답을 받기 전에 요청이 버려지면 span에 오류로 표시한다.
///
/// 시도 타임아웃(`tokio::time::timeout`)은 시간이 다 되면 기다리던 Future를 **그 자리에서 버린다.**
/// 버려진 Future는 `.await` 뒤의 코드를 실행하지 못하므로, 위의 "결과를 span에 남기는 코드"도 실행되지 않는다.
/// 값이 사라질 때는 `Drop`이 반드시 불리므로, 버려질 때 할 일은 여기에 적는다 (≈ C#의 `using` + `Dispose`).
struct Pending {
    span: Span,
    finished: bool,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.finished {
            self.span.record("otel.status_code", "error");
            self.span.record("error.type", "cancelled");
        }
    }
}
