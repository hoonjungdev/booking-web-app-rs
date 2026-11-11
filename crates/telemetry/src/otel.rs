//! OpenTelemetry provider 구성 (docs/05-observability-and-analytics.md §1)
//!
//! provider는 신호 종류마다 하나씩 있다 (≈ .NET의 `TracerProvider` / `MeterProvider` / `LoggerProvider`).
//!
//! | 신호 | provider | 내보내는 방식 |
//! |---|---|---|
//! | 추적 | [`SdkTracerProvider`] | 끝난 span을 모아 5초마다 (batch) |
//! | 메트릭 | [`SdkMeterProvider`] | 집계값을 10초마다 (periodic) |
//! | 로그 | [`SdkLoggerProvider`] | 모아서 1초마다 (batch) |
//!
//! 내보내기는 SDK가 만든 **전용 스레드**에서 한다. 요청을 처리하는 tokio 태스크는 메모리 큐에 넣기만 하므로,
//! 수집기(collector)가 느리거나 죽어 있어도 서비스가 느려지지 않는다. 큐가 가득 차면 버린다.

use std::time::Duration;

use opentelemetry::{KeyValue, global, trace::TracerProvider as _};
use opentelemetry_otlp::{ExporterBuildError, LogExporter, MetricExporter, SpanExporter};
use opentelemetry_sdk::{
    Resource,
    logs::SdkLoggerProvider,
    metrics::{PeriodicReader, SdkMeterProvider},
    propagation::TraceContextPropagator,
    trace::{SdkTracer, SdkTracerProvider},
};

/// 메트릭을 내보내는 간격. SDK 기본값(60초)은 로컬에서 대시보드를 보며 실험하기에 너무 길다.
/// 환경 변수 `OTEL_METRIC_EXPORT_INTERVAL`(밀리초)로 바꿀 수 있다.
const METRIC_EXPORT_INTERVAL: Duration = Duration::from_secs(10);
const METRIC_EXPORT_INTERVAL_ENV: &str = "OTEL_METRIC_EXPORT_INTERVAL";

/// 종료할 때 남은 데이터를 내보내는 데 쓰는 최대 시간 (수집기가 죽어 있어도 종료가 오래 걸리지 않게)
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Providers {
    tracer: SdkTracerProvider,
    /// `None`이면 메트릭을 내보내지 않는다 (계측 코드는 아무 일도 하지 않는 기본 구현을 쓰게 된다)
    meter: Option<SdkMeterProvider>,
    logger: Option<SdkLoggerProvider>,
}

impl Providers {
    /// OTLP(HTTP + protobuf)로 내보내는 구성.
    ///
    /// 주소는 환경 변수 `OTEL_EXPORTER_OTLP_ENDPOINT`로 정한다 (기본값 `http://localhost:4318`).
    /// 받는 쪽을 바꿔도 코드는 그대로다 (docs/decisions.md D-06).
    pub(crate) fn otlp(
        service_name: &'static str,
        service_version: &'static str,
    ) -> Result<Self, ExporterBuildError> {
        let resource = resource(service_name, service_version);

        let tracer = SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_batch_exporter(SpanExporter::builder().with_http().build()?)
            .build();

        let reader = PeriodicReader::builder(MetricExporter::builder().with_http().build()?)
            .with_interval(metric_export_interval())
            .build();
        let meter = SdkMeterProvider::builder()
            .with_resource(resource.clone())
            .with_reader(reader)
            .build();

        let logger = SdkLoggerProvider::builder()
            .with_resource(resource)
            .with_batch_exporter(LogExporter::builder().with_http().build()?)
            .build();

        Ok(Self {
            tracer,
            meter: Some(meter),
            logger: Some(logger),
        })
    }

    /// 아무 데도 내보내지 않는 구성. span은 만들자마자 버리지만 **trace ID는 만든다**.
    /// 그래서 내보내기를 꺼도 로그 · 응답 헤더(`X-Trace-Id`) · DB에 남는 trace ID는 그대로 쓸 수 있다.
    pub(crate) fn local(service_name: &'static str, service_version: &'static str) -> Self {
        Self {
            tracer: SdkTracerProvider::builder()
                .with_resource(resource(service_name, service_version))
                .build(),
            meter: None,
            logger: None,
        }
    }

    /// 테스트용: 이미 만든 provider를 묶는다
    #[cfg(feature = "testing")]
    pub(crate) fn from_parts(tracer: SdkTracerProvider, meter: SdkMeterProvider) -> Self {
        Self {
            tracer,
            meter: Some(meter),
            logger: None,
        }
    }

    /// 다른 크레이트의 계측 코드가 찾아 쓸 수 있게 전역으로 등록한다.
    ///
    /// - 메트릭: `providers`, `booking-server`의 계측 코드는 `opentelemetry::global::meter(..)`로 계측기를 만든다.
    /// - 전파 형식: W3C Trace Context (`traceparent` 헤더). 등록하지 않으면 헤더를 읽지도 쓰지도 않는다.
    pub(crate) fn install_globals(&self) {
        global::set_tracer_provider(self.tracer.clone());
        if let Some(meter) = &self.meter {
            global::set_meter_provider(meter.clone());
        }
        global::set_text_map_propagator(TraceContextPropagator::new());
    }

    /// tracing span을 OpenTelemetry span으로 바꿀 때 쓰는 tracer
    pub(crate) fn tracer(&self, name: &'static str) -> SdkTracer {
        self.tracer.tracer(name)
    }

    pub(crate) fn logger(&self) -> Option<&SdkLoggerProvider> {
        self.logger.as_ref()
    }

    /// 아직 보내지 않은 데이터를 내보내고 전용 스레드를 멈춘다.
    ///
    /// 실패해도(수집기가 떠 있지 않을 때 등) 종료를 막지 않고 경고만 남긴다. 로그 provider를 **마지막에** 닫는다.
    /// 그래야 앞 단계의 실패를 알리는 경고까지 로그로 내보낼 수 있다.
    pub(crate) fn shutdown(&self) {
        if let Err(error) = self.tracer.shutdown_with_timeout(SHUTDOWN_TIMEOUT) {
            tracing::warn!(%error, "남은 추적 데이터를 내보내지 못했다");
        }
        if let Some(meter) = &self.meter
            && let Err(error) = meter.shutdown_with_timeout(SHUTDOWN_TIMEOUT)
        {
            tracing::warn!(%error, "남은 메트릭을 내보내지 못했다");
        }
        if let Some(logger) = &self.logger {
            // 실패해도 따로 알리지 않는다 (`let _ =`: 결과를 일부러 버린다). 내보내기 실패는 SDK가 이미 콘솔에 남겼고,
            // provider를 닫은 뒤에 로그를 남기면 SDK가 "닫힌 뒤에 로그가 들어왔다"고 또 경고한다.
            let _ = logger.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
        }
    }
}

/// 메트릭을 내보내는 간격. 환경 변수가 0보다 큰 정수(밀리초)면 그 값을, 없거나 잘못된 값이면 기본값을 쓴다.
fn metric_export_interval() -> Duration {
    std::env::var(METRIC_EXPORT_INTERVAL_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map_or(METRIC_EXPORT_INTERVAL, Duration::from_millis)
}

/// 이 프로세스가 무엇인지 설명하는 속성. 모든 span · 메트릭 · 로그에 함께 붙는다.
/// Grafana에서는 `service.name`으로 booking-server와 mock-provider를 구분한다 (Prometheus에서는 `job` 라벨).
///
/// 같은 서비스의 프로세스를 서로 구분하는 속성(`service.instance.id`)은 넣지 않았다. 지금은 서비스마다 프로세스가
/// 하나다. API와 워커를 따로 띄우거나 여러 대로 늘리면 같은 시계열에 여러 프로세스가 쓰게 되므로, 그때 추가한다.
fn resource(service_name: &'static str, service_version: &'static str) -> Resource {
    Resource::builder()
        .with_service_name(service_name)
        .with_attribute(KeyValue::new("service.version", service_version))
        .build()
}
