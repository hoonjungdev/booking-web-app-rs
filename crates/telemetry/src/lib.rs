//! tracing · OpenTelemetry 초기화와 공용 계측 도구 (booking-server와 mock-provider가 공유)
//!
//! ```text
//! tracing 매크로 (info!, #[instrument] …)
//!    ├─▶ 콘솔 로그 (stderr)
//!    ├─▶ span  ──▶ OpenTelemetry 추적 ─┐
//!    └─▶ event ──▶ OpenTelemetry 로그 ─┼─ OTLP ─▶ 수집기 (로컬: grafana/otel-lgtm)
//! 계측기 (Counter, Histogram …) ──────┘
//! ```
//!
//! | 모듈 | 역할 |
//! |---|---|
//! | `otel` | OTLP exporter와 provider 구성 |
//! | `filter` | 전역 필터(로그는 `RUST_LOG`, span은 항상)와 로그 되먹임 막기 |
//! | `http` | 서버 요청 계측 미들웨어 ([`HttpTelemetryLayer`]) |
//! | `propagation` | trace 정보를 HTTP 헤더 · 작업 큐로 실어 나르기 |
//! | `runtime` | tokio 런타임 지표 |
//! | `testing` | 테스트에서 span · 메트릭을 메모리에 모아 확인하기 (`testing` feature) |
//!
//! 코드에서는 계속 `tracing` 매크로만 쓴다. OpenTelemetry로 보내는 일은 이 크레이트가 설치한 layer가 한다
//! (≈ .NET에서 코드는 `ILogger` · `Activity`만 쓰고, 어디로 내보낼지는 시작할 때 한 번 구성하는 것).

mod filter;
mod http;
mod otel;
mod propagation;
mod runtime;
#[cfg(feature = "testing")]
pub mod testing;

use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use tracing_subscriber::{
    Layer, Registry, fmt,
    layer::SubscriberExt,
    util::{SubscriberInitExt, TryInitError},
};

pub use http::{HttpTelemetry, HttpTelemetryLayer, TRACE_ID_HEADER};
pub use propagation::{
    TraceCarrier, current_carrier, current_trace_id, link_to_origin, trace_id_of,
};
pub use runtime::register_runtime_metrics;

#[cfg(any(not(feature = "tokio-console"), feature = "testing"))]
use filter::GlobalFilter;
use filter::WithoutExporterLogs;
use otel::Providers;

/// 초 단위 히스토그램의 버킷 경계 (docs/05-observability-and-analytics.md §4).
///
/// OpenTelemetry의 기본 버킷(0, 5, 10, 25 … 10000)은 밀리초 값에 맞춰져 있다. 초 단위 값을 그대로 넣으면
/// 거의 모든 값이 첫 버킷에 몰려 p95를 구할 수 없으므로, 히스토그램을 만들 때 이 경계를 직접 지정한다.
pub const SECONDS_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// OpenTelemetry 표준 환경 변수. `true`면 아무것도 내보내지 않는다.
const SDK_DISABLED_ENV: &str = "OTEL_SDK_DISABLED";

/// 여러 종류의 layer를 한 Vec에 담기 위한 trait 객체 (형식이 Compact인지 Json인지에 따라 layer 타입이 달라진다)
type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// 로그 출력 형식
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// 사람이 읽기 쉬운 한 줄 형식 (로컬 개발 기본값)
    Compact,
    /// 한 줄에 JSON 하나 (로그 수집기용)
    Json,
}

impl LogFormat {
    /// 환경 변수 `LOG_FORMAT`이 `json`이면 Json, 그 밖에는 Compact.
    pub fn from_env() -> Self {
        match std::env::var("LOG_FORMAT") {
            Ok(v) if v.eq_ignore_ascii_case("json") => Self::Json,
            _ => Self::Compact,
        }
    }
}

/// 추적 · 메트릭 · 로그를 밖으로 내보낼지
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Export {
    /// OTLP로 내보낸다. 단, 환경 변수 `OTEL_SDK_DISABLED=true`면 내보내지 않는다.
    Otlp,
    /// 내보내지 않는다 (콘솔 로그만). 금방 끝나는 명령에 쓴다.
    Off,
}

/// [`init`]에 넘기는 설정
#[derive(Debug, Clone, Copy)]
pub struct Settings<'a> {
    /// 서비스 이름 (`service.name`). Grafana에서 이 이름으로 프로세스를 구분한다.
    pub service_name: &'static str,
    pub service_version: &'static str,
    /// `RUST_LOG`가 없을 때 쓸 로그 필터 (예: `"info,mock_provider=debug"`)
    pub default_filter: &'a str,
    pub log_format: LogFormat,
    pub export: Export,
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("OTLP exporter를 만들 수 없습니다: {0}")]
    Exporter(#[from] opentelemetry_otlp::ExporterBuildError),
    #[error("전역 로거를 설치할 수 없습니다 (이미 설치되어 있습니다): {0}")]
    Subscriber(#[from] TryInitError),
}

/// [`init`]이 돌려주는 손잡이. **프로그램이 끝날 때까지 들고 있어야 한다.**
///
/// 값이 사라질 때(`Drop`) 아직 보내지 않은 추적 · 메트릭 · 로그를 마저 내보내고 정리한다
/// (≈ C#에서 `using var provider = ...`로 잡아 두고 끝날 때 `Dispose`되는 것).
/// `let _ = telemetry::init(..)`처럼 받으면 그 줄에서 바로 사라지므로, `let _telemetry = ...`처럼 이름을 붙여 둔다.
#[must_use = "변수에 담아 프로그램이 끝날 때까지 들고 있어야 한다 (`let _telemetry = ...`). 버리면 그 자리에서 내보내기가 멈춘다"]
pub struct Telemetry {
    providers: Providers,
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.providers.shutdown();
    }
}

/// 전역 로거와 OpenTelemetry를 설치한다. 프로세스 시작 시 한 번만 부른다.
///
/// | 환경 변수 | 뜻 | 기본값 |
/// |---|---|---|
/// | `RUST_LOG` | 로그 필터. 콘솔 로그와 내보내는 로그에 함께 적용된다. span은 이 값과 상관없이 info 이상이면 항상 만든다 | `settings.default_filter` |
/// | `LOG_FORMAT` | `json`이면 콘솔 로그를 JSON으로 | 한 줄 형식 |
/// | `OTEL_EXPORTER_OTLP_ENDPOINT` | 수집기 주소 (OTLP/HTTP) | `http://localhost:4318` |
/// | `OTEL_SDK_DISABLED` | `true`면 내보내지 않는다 | `false` |
/// | `OTEL_METRIC_EXPORT_INTERVAL` | 메트릭을 내보내는 간격 (밀리초) | `10000` |
///
/// 콘솔 로그는 표준 에러(stderr)로 쓴다. 표준 출력(stdout)은 명령의 결과(예: 발급한 API 키)에 쓰기 위해 비워 둔다.
///
/// 수집기가 떠 있지 않아도 서비스는 정상으로 돈다. 내보내기 실패는 콘솔에 경고로만 남는다.
pub fn init(settings: Settings<'_>) -> Result<Telemetry, InitError> {
    let export = settings.export == Export::Otlp && !sdk_disabled();
    let providers = if export {
        Providers::otlp(settings.service_name, settings.service_version)?
    } else {
        Providers::local(settings.service_name, settings.service_version)
    };
    providers.install_globals();
    let layers = layers(settings.log_format, &providers, settings.service_name);
    // `#[cfg(..)]`: 조건에 맞을 때만 그 줄이 컴파일된다 (≈ C#의 `#if`). feature를 켰는지에 따라 둘 중 한 줄만 남는다.
    #[cfg(not(feature = "tokio-console"))]
    install_subscriber(layers, GlobalFilter::from_env(settings.default_filter))?;
    #[cfg(feature = "tokio-console")]
    install_subscriber_with_tokio_console(layers, settings.default_filter)?;
    Ok(Telemetry { providers })
}

fn sdk_disabled() -> bool {
    std::env::var(SDK_DISABLED_ENV).is_ok_and(|value| value.eq_ignore_ascii_case("true"))
}

/// span · 이벤트를 받아 처리할 layer들: 콘솔 로그, OpenTelemetry 추적, (내보낼 때만) OpenTelemetry 로그.
///
/// layer는 "span · 이벤트를 받는 쪽"이다. 여러 개를 쌓으면 하나의 span · 이벤트가 모두에게 전달된다
/// (≈ `ILoggerProvider`를 여러 개 등록하는 것).
fn layers(format: LogFormat, providers: &Providers, service_name: &'static str) -> Vec<BoxedLayer> {
    let mut layers = vec![
        console_layer(format),
        // tracing span → OpenTelemetry span
        tracing_opentelemetry::layer()
            .with_tracer(providers.tracer(service_name))
            // 스레드 이름 · ID 속성은 비동기 코드에서 의미가 적어 뺀다 (태스크가 스레드를 옮겨 다닌다)
            .with_threads(false)
            .boxed(),
    ];
    if let Some(logger) = providers.logger() {
        // tracing 이벤트 → OpenTelemetry 로그. 지금 span의 trace ID · span ID가 로그에 함께 붙어서
        // Grafana에서 로그와 추적을 오갈 수 있다. 로그를 내보내는 과정에서 생긴 로그는 빼고 넘긴다 (되먹임 방지).
        let bridge = OpenTelemetryTracingBridge::new(logger);
        layers.push(WithoutExporterLogs::new(bridge).boxed());
    }
    layers
}

/// layer들과 필터를 묶어 전역 subscriber로 설치한다.
///
/// 필터는 **전역**으로 건다: 필터에 걸린 span · 이벤트는 어느 layer에도 가지 않는다.
/// 그래서 콘솔에 찍히는 로그와 Grafana로 가는 로그가 항상 같다. 무엇을 통과시키는지는 [`GlobalFilter`] 참고.
#[cfg(any(not(feature = "tokio-console"), feature = "testing"))]
fn install_subscriber(layers: Vec<BoxedLayer>, filter: GlobalFilter) -> Result<(), TryInitError> {
    tracing_subscriber::registry()
        .with(layers)
        .with(filter)
        .try_init()
}

/// tokio-console 개발 모드의 subscriber (docs/05-observability-and-analytics.md §9).
///
/// tokio-console은 런타임 내부 이벤트(`tokio=trace`)를 `RUST_LOG`와 상관없이 모두 받아야 한다. 전역 필터를 쓰면
/// 그 이벤트가 걸러지므로, 여기서는 우리 layer들에만 필터를 건다 (per-layer filter).
///
/// **평소에는 이 방식을 쓰지 않는 이유**: per-layer filter는 "이번 이벤트를 어느 layer가 껐는지"를 스레드별 상태에
/// 적어 둔다. 그런데 이벤트를 남기지 않고 "이 수준이 켜져 있나?"만 묻는 코드(`tracing::enabled!`, sqlx가 쿼리마다 쓴다)가
/// 있으면 그 상태가 지워지지 않고 남아, 같은 스레드의 **다음** span · 이벤트가 엉뚱하게 버려진다.
/// 아래의 `dynamic_filter_fn`은 매번 필터를 다시 평가하게 만들어 남은 상태를 덮어쓰는 우회책이다
/// (그만큼 느려지므로 개발 모드에서만 쓴다). 이 모드에서는 span도 `RUST_LOG`를 따른다.
#[cfg(feature = "tokio-console")]
fn install_subscriber_with_tokio_console(
    layers: Vec<BoxedLayer>,
    default_filter: &str,
) -> Result<(), TryInitError> {
    use tracing_subscriber::filter::{FilterExt, dynamic_filter_fn};

    let filter = filter::env_filter(default_filter).and(dynamic_filter_fn(|_, _| true));
    tracing_subscriber::registry()
        .with(layers.with_filter(filter))
        // 127.0.0.1:6669에서 tokio-console의 접속을 기다린다
        .with(console_subscriber::spawn())
        .try_init()
}

fn console_layer(format: LogFormat) -> BoxedLayer {
    match format {
        LogFormat::Compact => fmt::layer().compact().with_writer(std::io::stderr).boxed(),
        LogFormat::Json => fmt::layer()
            .json()
            .flatten_event(true)
            .with_writer(std::io::stderr)
            .boxed(),
    }
}
