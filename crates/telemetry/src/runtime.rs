//! tokio 런타임 지표 (docs/05-observability-and-analytics.md §4)
//!
//! 런타임이 얼마나 바쁜지 보여 준다. 요청이 느린데 업체도 DB도 멀쩡하다면 여기부터 본다:
//! 대기열(`global_queue_depth`)이 계속 쌓여 있으면 워커 스레드를 오래 붙잡는 코드(블로킹 호출)가 있다는 신호다.
//! 어느 태스크가 문제인지는 tokio-console로 찾는다 (같은 문서 §9).

use opentelemetry::global;
use tokio::runtime::Handle;

/// tokio 런타임 지표를 등록한다. 런타임 안에서(= `#[tokio::main]` 함수 안에서) 불러야 한다.
///
/// 여기서 쓰는 것은 **관측형(observable)** 계측기다. 값을 그때그때 기록하는 대신, 지표를 내보낼 때마다
/// SDK가 콜백을 불러 현재 값을 읽어 간다 (≈ .NET의 `Meter.CreateObservableGauge`).
/// 콜백은 SDK의 내보내기 스레드에서 실행되므로 요청 처리 경로에는 비용이 들지 않는다.
pub fn register_runtime_metrics() {
    let Ok(handle) = Handle::try_current() else {
        tracing::warn!("tokio 런타임 밖에서 불려 런타임 지표를 등록하지 않는다");
        return;
    };
    let meter = global::meter("telemetry");

    // `move` 클로저가 `metrics`를 가져가므로, 콜백마다 복제본을 하나씩 준다
    let metrics = handle.metrics();
    let _workers = meter
        .u64_observable_gauge("tokio.runtime.workers")
        .with_description("런타임의 워커 스레드 수")
        .with_callback({
            let metrics = metrics.clone();
            move |observer| observer.observe(metrics.num_workers() as u64, &[])
        })
        .build();
    let _alive_tasks = meter
        .u64_observable_gauge("tokio.runtime.alive_tasks")
        .with_description("끝나지 않은 태스크 수")
        .with_callback({
            let metrics = metrics.clone();
            move |observer| observer.observe(metrics.num_alive_tasks() as u64, &[])
        })
        .build();
    let _queue_depth = meter
        .u64_observable_gauge("tokio.runtime.global_queue_depth")
        .with_description("워커 스레드를 기다리는 태스크 수 (전역 대기열)")
        .with_callback(move |observer| observer.observe(metrics.global_queue_depth() as u64, &[]))
        .build();
}
