//! 접수 요청과 작업 처리를 trace로 잇기 (docs/05-observability-and-analytics.md §3)
//!
//! ```text
//! POST /v1/bookings ── trace A ──▶ jobs.trace_context에 "trace A의 어느 span"인지 저장
//!                                        │ (몇 초 ~ 몇 분 뒤, 다른 태스크에서)
//! job.submit_booking ── trace B ──▶ 시작할 때 trace A로 가는 link를 건다
//! ```
//! 같은 trace로 잇지 않고 link로 잇는 이유: 작업은 한참 뒤에 여러 번 다시 실행될 수 있다. 한 trace에 넣으면
//! 접수 요청 trace가 끝나지 않고 계속 자란다. link로 이으면 Grafana에서 두 trace를 서로 오갈 수 있다.

use db::{TraceRef, jobs::Job};
use telemetry::TraceCarrier;
use tracing::Span;

/// 지금 실행 중인 span의 trace 정보를 DB에 남길 형태로 꺼낸다.
///
/// trace가 없으면(OpenTelemetry를 설치하지 않은 테스트 등) 빈 값이 나오고, DB에는 NULL로 남는다.
pub fn current() -> TraceRef {
    let carrier = telemetry::current_carrier();
    TraceRef {
        trace_id: telemetry::current_trace_id(),
        // 문자열 맵이라 JSON으로 바꾸는 데 실패할 일은 없다. 그래도 실패하면 trace 정보만 빼고 예약 처리는 계속한다.
        context: (!carrier.is_empty())
            .then(|| serde_json::to_value(&carrier).ok())
            .flatten(),
    }
}

/// `span`(작업을 처리하는 span)에서 그 작업을 등록한 쪽으로 가는 link를 건다.
/// 등록할 때 trace가 없었거나 저장된 값을 읽을 수 없으면 link 없이 진행한다.
pub fn link_to_origin(span: &Span, job: &Job) {
    let Some(context) = &job.trace_context else {
        return;
    };
    match serde_json::from_value::<TraceCarrier>(context.clone()) {
        Ok(carrier) => telemetry::link_to_origin(span, &carrier),
        Err(error) => {
            tracing::debug!(%error, "작업의 trace 문맥을 읽을 수 없어 link를 걸지 않는다");
        }
    }
}
