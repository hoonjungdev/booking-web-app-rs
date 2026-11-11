//! trace 정보를 프로세스 · 작업 사이로 실어 나르기 (docs/05-observability-and-analytics.md §3)
//!
//! trace는 "요청 하나가 거쳐 간 길"이다. 길이 다른 프로세스나 나중에 실행되는 작업으로 이어지려면
//! 지금 어느 trace의 어느 span인지(W3C Trace Context의 `traceparent`)를 함께 넘겨야 한다.
//!
//! | 경계 | 실어 보내는 곳 | 받는 쪽에서 |
//! |---|---|---|
//! | HTTP | 요청 헤더 `traceparent` | **부모**로 삼는다 → 같은 trace로 이어짐 |
//! | 작업 큐 | `jobs.trace_context` (JSON) | **link**로 건다 → 다른 trace지만 서로 오갈 수 있음 |
//!
//! 작업은 몇 분 뒤에 실행될 수도 있어서 같은 trace에 붙이지 않는다. 붙이면 접수 요청 trace가 끝없이 길어진다.

use std::collections::HashMap;

use axum::http::HeaderMap;
use opentelemetry::{Context, global, trace::TraceContextExt};
use opentelemetry_http::HeaderExtractor;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// 다른 곳으로 실어 보낼 trace 정보. `{"traceparent": "00-<trace id>-<span id>-01"}` 꼴이다.
/// 비어 있으면 실어 보낼 trace가 없다는 뜻이다 (OpenTelemetry를 쓰지 않는 테스트 등).
pub type TraceCarrier = HashMap<String, String>;

/// 지금 실행 중인 span의 trace ID (16진수 32자). trace가 없으면 `None`.
pub fn current_trace_id() -> Option<String> {
    trace_id_of(&Span::current())
}

/// `span`의 trace ID. trace가 없으면 `None`.
///
/// 이 함수를 부르면 span의 부모가 확정된다. 부모를 바꾸려면([`set_remote_parent`]) 이 함수보다 먼저 불러야 한다.
pub fn trace_id_of(span: &Span) -> Option<String> {
    let context = span.context();
    let span_context = context.span().span_context().clone();
    span_context
        .is_valid()
        .then(|| span_context.trace_id().to_string())
}

/// 지금 실행 중인 span의 trace 정보를 실어 보낼 수 있는 형태로 꺼낸다 (작업 큐에 저장할 때 쓴다).
pub fn current_carrier() -> TraceCarrier {
    let context = Span::current().context();
    let mut carrier = TraceCarrier::new();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut carrier);
    });
    // 전파기는 값이 없는 항목(`tracestate`)도 빈 문자열로 넣는다. 저장할 때는 뺀다.
    carrier.retain(|_, value| !value.is_empty());
    carrier
}

/// `span`에 "이 일을 시킨 쪽"으로 가는 link를 건다. 실려 온 정보가 없거나 올바르지 않으면 아무것도 하지 않는다.
pub fn link_to_origin(span: &Span, carrier: &TraceCarrier) {
    // 빈 Context에서 시작한다. `extract`는 "지금 Context"에 덧붙이는데, 실려 온 정보가 없으면 지금 span
    // (= link를 걸려는 바로 그 span)이 그대로 나와서 자기 자신에게 link를 걸게 된다.
    let origin = global::get_text_map_propagator(|propagator| {
        propagator.extract_with_context(&Context::new(), carrier)
    });
    let origin = origin.span().span_context().clone();
    if origin.is_valid() {
        span.add_link(origin);
    }
}

/// 요청 헤더(`traceparent`)에 실려 온 trace를 `span`의 부모로 삼는다. 헤더가 없으면 `span`은 새 trace의 시작이 된다.
///
/// span을 만든 직후, 들어가기(enter) 전에 불러야 한다.
pub(crate) fn set_remote_parent(span: &Span, headers: &HeaderMap) {
    // 여기서도 빈 Context에서 시작한다 (위 `link_to_origin`과 같은 이유: 헤더가 없을 때 엉뚱한 부모를 물려받지 않게).
    let parent = global::get_text_map_propagator(|propagator| {
        propagator.extract_with_context(&Context::new(), &HeaderExtractor(headers))
    });
    if parent.span().span_context().is_valid() {
        // OpenTelemetry layer가 없거나(테스트) 필터로 꺼진 span이면 실패한다. 그때는 부모를 붙일 trace 자체가 없다.
        span.set_parent(parent).ok();
    }
}
