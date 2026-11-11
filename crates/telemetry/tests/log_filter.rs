//! 로그를 줄여도 추적은 유지되는지 (`telemetry::filter::GlobalFilter`, docs/decisions.md D-22)
//!
//! 전역 subscriber는 프로세스에 하나뿐이고 이 테스트는 다른 필터(`warn`)가 필요해서, 파일을 따로 두었다
//! (`tests/` 아래의 파일은 각각 별도 프로세스로 돈다).

use tracing::Level;

#[test]
fn 로그를_warn으로_줄여도_span과_trace_id는_만들어진다() {
    // 운영에서 로그를 줄이려고 `RUST_LOG=warn`으로 띄운 상황
    let captured = telemetry::testing::install("warn");

    // 이벤트(로그)는 필터를 따른다
    assert!(tracing::event_enabled!(Level::WARN));
    assert!(!tracing::event_enabled!(Level::INFO));
    // span은 info 이상이면 필터와 상관없이 만든다. 더 자세한 span은 필터를 따른다.
    assert!(tracing::span_enabled!(Level::INFO));
    assert!(!tracing::span_enabled!(Level::DEBUG));

    let span = tracing::info_span!("quiet-but-traced");
    assert!(!span.is_disabled());
    let trace_id = telemetry::trace_id_of(&span)
        .expect("로그 수준과 상관없이 trace ID가 있어야 응답 헤더와 DB에 남길 수 있다");
    span.in_scope(|| {
        tracing::info!("이 로그는 걸러진다");
        tracing::warn!("이 로그는 남는다");
    });
    drop(span);

    let spans = captured.spans();
    let exported = spans
        .iter()
        .find(|span| span.trace_id == trace_id)
        .expect("span이 내보내져야 한다");
    assert_eq!(exported.name, "quiet-but-traced");
    let logged: Vec<_> = exported
        .events
        .iter()
        .map(|event| event.name.as_str())
        .collect();
    assert_eq!(logged, vec!["이 로그는 남는다"]);
}
