//! 테스트에서 span과 메트릭을 메모리에 모아 확인하는 도구 (`testing` feature)
//!
//! 수집기(Grafana)를 띄우지 않고도 "trace가 서비스 사이로 이어지는가", "지표가 기대한 라벨로 기록되는가"를
//! 테스트로 검증할 수 있다. 실제 구성과 다른 점은 내보내는 곳뿐이다 (OTLP 대신 메모리).

use std::{collections::BTreeMap, sync::OnceLock, time::Duration};

use opentelemetry::{
    KeyValue,
    trace::{SpanKind, Status},
};
use opentelemetry_sdk::{
    metrics::{
        InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
        data::{AggregatedMetrics, MetricData},
    },
    trace::{InMemorySpanExporter, SdkTracerProvider, SpanData},
};

use crate::{LogFormat, filter::GlobalFilter, install_subscriber, layers, otel::Providers};

/// 프로세스에 하나만 설치한다. 전역 subscriber는 한 번만 설치할 수 있기 때문이다.
/// `OnceLock`: 처음 한 번만 초기화되는 전역 값 (≈ C#의 `Lazy<T>`).
static CAPTURED: OnceLock<Captured> = OnceLock::new();

/// 메모리에 모인 span과 메트릭
pub struct Captured {
    spans: InMemorySpanExporter,
    metrics: InMemoryMetricExporter,
    meter_provider: SdkMeterProvider,
    /// 들고 있어야 한다: provider가 사라지면 span 수집이 멈춘다
    _providers: Providers,
}

/// 끝난 span 하나 (테스트에서 보기 쉽게 문자열로 풀어 놓은 것)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanView {
    pub name: String,
    pub trace_id: String,
    pub span_id: String,
    /// 부모가 없는 span(trace의 시작)이면 `None`
    pub parent_span_id: Option<String>,
    /// `server`, `client`, `internal` …
    pub kind: &'static str,
    pub attributes: BTreeMap<String, String>,
    /// link로 가리키는 trace들의 ID
    pub linked_trace_ids: Vec<String>,
    /// 이 span 안에서 남긴 로그 (tracing 이벤트는 span의 이벤트로도 기록된다)
    pub events: Vec<EventView>,
    pub is_error: bool,
}

/// span 안에서 남긴 로그 한 줄
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventView {
    /// 로그 메시지
    pub name: String,
    /// 로그에 붙인 필드
    pub attributes: BTreeMap<String, String>,
}

/// 메트릭의 한 시계열(라벨 조합 하나)의 현재 값
#[derive(Debug, Clone, PartialEq)]
pub struct MetricPoint {
    pub attributes: BTreeMap<String, String>,
    /// counter: 누적 합계, gauge: 마지막 값, histogram: 기록한 횟수
    pub value: f64,
    /// histogram의 버킷 경계 (다른 종류면 비어 있음)
    pub bounds: Vec<f64>,
}

/// 메모리로 모으는 OpenTelemetry와 전역 subscriber를 설치한다. 여러 번 불러도 처음 한 번만 설치한다.
///
/// `log_filter`는 실제 서버가 쓰는 기본 필터를 넘긴다. 그래야 "실제 설정에서 보이는 span"을 검증하게 된다.
/// 환경 변수 `RUST_LOG`는 보지 않는다 (테스트 결과가 실행 환경에 따라 달라지지 않게).
///
/// # Panics
/// 다른 전역 subscriber가 이미 설치되어 있으면 패닉한다 (테스트 준비 오류).
pub fn install(log_filter: &str) -> &'static Captured {
    CAPTURED.get_or_init(|| {
        let spans = InMemorySpanExporter::default();
        // simple exporter: span이 끝나는 즉시 내보낸다 (테스트에서 기다릴 필요가 없다)
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(spans.clone())
            .build();

        let metrics = InMemoryMetricExporter::default();
        // 주기를 길게 잡아 저절로는 내보내지 않게 하고, 값을 읽을 때 직접 내보낸다 (`Captured::metric`)
        let reader = PeriodicReader::builder(metrics.clone())
            .with_interval(Duration::from_secs(3600))
            .build();
        let meter_provider = SdkMeterProvider::builder().with_reader(reader).build();

        let providers = Providers::from_parts(tracer_provider, meter_provider.clone());
        providers.install_globals();
        // 실제 서버와 같은 구성(전역 필터)으로 설치한다. tokio-console layer는 넣지 않는다.
        install_subscriber(
            layers(LogFormat::Compact, &providers, "test"),
            GlobalFilter::exact(log_filter),
        )
        .expect("테스트 프로세스에는 다른 전역 subscriber가 없어야 한다");
        Captured {
            spans,
            metrics,
            meter_provider,
            _providers: providers,
        }
    })
}

impl Captured {
    /// 지금까지 끝난 span 전부 (끝난 순서대로)
    pub fn spans(&self) -> Vec<SpanView> {
        self.spans
            .get_finished_spans()
            .expect("메모리 exporter는 실패하지 않는다")
            .iter()
            .map(SpanView::from)
            .collect()
    }

    /// 이름이 `name`인 메트릭의 시계열들. 한 번도 기록되지 않았으면 비어 있다.
    pub fn metric(&self, name: &str) -> Vec<MetricPoint> {
        // 누적 값을 내보내므로 마지막으로 내보낸 것만 보면 된다
        self.metrics.reset();
        self.meter_provider
            .force_flush()
            .expect("메모리 exporter는 실패하지 않는다");
        let exported = self
            .metrics
            .get_finished_metrics()
            .expect("메모리 exporter는 실패하지 않는다");

        let mut points = Vec::new();
        for resource_metrics in &exported {
            for scope_metrics in resource_metrics.scope_metrics() {
                for metric in scope_metrics.metrics().filter(|m| m.name() == name) {
                    points.extend(match metric.data() {
                        AggregatedMetrics::F64(data) => points_of(data, |v| v),
                        AggregatedMetrics::U64(data) => points_of(data, |v| v as f64),
                        AggregatedMetrics::I64(data) => points_of(data, |v| v as f64),
                    });
                }
            }
        }
        points
    }
}

impl SpanView {
    /// 속성 값 (없으면 `None`)
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }
}

impl MetricPoint {
    /// 라벨 값 (없으면 `None`)
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }
}

impl From<&SpanData> for SpanView {
    fn from(span: &SpanData) -> Self {
        let no_parent = span.parent_span_id == opentelemetry::trace::SpanId::INVALID;
        Self {
            name: span.name.to_string(),
            trace_id: span.span_context.trace_id().to_string(),
            span_id: span.span_context.span_id().to_string(),
            parent_span_id: (!no_parent).then(|| span.parent_span_id.to_string()),
            kind: match span.span_kind {
                SpanKind::Client => "client",
                SpanKind::Server => "server",
                SpanKind::Producer => "producer",
                SpanKind::Consumer => "consumer",
                SpanKind::Internal => "internal",
            },
            attributes: attributes_of(span.attributes.iter()),
            linked_trace_ids: span
                .links
                .iter()
                .map(|link| link.span_context.trace_id().to_string())
                .collect(),
            events: span
                .events
                .iter()
                .map(|event| EventView {
                    name: event.name.to_string(),
                    attributes: attributes_of(event.attributes.iter()),
                })
                .collect(),
            is_error: matches!(span.status, Status::Error { .. }),
        }
    }
}

fn attributes_of<'a>(attributes: impl Iterator<Item = &'a KeyValue>) -> BTreeMap<String, String> {
    attributes
        .map(|kv| (kv.key.as_str().to_owned(), kv.value.as_str().into_owned()))
        .collect()
}

/// 값 타입(`f64`, `u64`, `i64`)마다 같은 일을 하므로 제네릭으로 쓴다. `to_f64`는 그 타입을 `f64`로 바꾸는 함수다.
fn points_of<T: Copy>(data: &MetricData<T>, to_f64: impl Fn(T) -> f64) -> Vec<MetricPoint> {
    match data {
        MetricData::Gauge(gauge) => gauge
            .data_points()
            .map(|point| MetricPoint {
                attributes: attributes_of(point.attributes()),
                value: to_f64(point.value()),
                bounds: Vec::new(),
            })
            .collect(),
        MetricData::Sum(sum) => sum
            .data_points()
            .map(|point| MetricPoint {
                attributes: attributes_of(point.attributes()),
                value: to_f64(point.value()),
                bounds: Vec::new(),
            })
            .collect(),
        MetricData::Histogram(histogram) => histogram
            .data_points()
            .map(|point| MetricPoint {
                attributes: attributes_of(point.attributes()),
                value: point.count() as f64,
                bounds: point.bounds().collect(),
            })
            .collect(),
        // 지수 히스토그램은 쓰지 않는다
        MetricData::ExponentialHistogram(_) => Vec::new(),
    }
}
