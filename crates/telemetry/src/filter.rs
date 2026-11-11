//! 무엇을 받아들일지 정하는 두 거름 장치 (docs/decisions.md D-22)
//!
//! | | 거르는 대상 | 규칙 |
//! |---|---|---|
//! | [`GlobalFilter`] | 모든 layer로 가는 span · 이벤트 | 이벤트(로그)는 `RUST_LOG`를 따르고, span은 info 이상이면 항상 통과 |
//! | [`WithoutExporterLogs`] | OpenTelemetry 로그로 나가는 이벤트 | 로그를 내보내는 과정에서 생긴 로그는 뺀다 |
//!
//! 둘 다 tracing-subscriber의 per-layer filter(`Layer::with_filter`)를 쓰지 않고 평범한 [`Layer`]로 만들었다.
//! per-layer filter는 스레드별 상태를 쓰는데, 그 상태가 sqlx의 `tracing::enabled!` 검사와 만나면
//! span · 로그가 무작위로 빠진다.

use tracing::{Event, Subscriber};
use tracing_subscriber::{EnvFilter, Layer, layer::Context};

/// 이 대상(target)으로 시작하는 이벤트는 OpenTelemetry 로그로 내보내지 않는다:
/// OpenTelemetry 자신의 진단 로그와, exporter가 쓰는 HTTP 클라이언트(reqwest · hyper · h2)의 로그.
const EXPORTER_TARGETS: [&str; 4] = ["opentelemetry", "reqwest", "hyper", "h2"];

/// `RUST_LOG`가 있으면 그 값으로, 없으면 `default_filter`로 필터를 만든다.
pub(crate) fn env_filter(default_filter: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter))
}

// 평소 빌드와 테스트가 쓰는 전역 필터. tokio-console 모드는 per-layer filter를 쓰므로(lib.rs 참고)
// 그 모드로만 빌드할 때는 이 모듈을 컴파일하지 않는다 (쓰지 않는 코드라는 경고가 난다).
#[cfg(any(not(feature = "tokio-console"), feature = "testing"))]
pub(crate) use global::GlobalFilter;

#[cfg(any(not(feature = "tokio-console"), feature = "testing"))]
mod global {
    use tracing::{
        Level, Metadata, Subscriber, level_filters::LevelFilter, span, subscriber::Interest,
    };
    use tracing_subscriber::{EnvFilter, Layer, layer::Context};

    /// 전역 필터. **이벤트(로그)는 `RUST_LOG`를 따르고, span은 info 이상이면 항상 통과**시킨다.
    ///
    /// span까지 `RUST_LOG`에 맡기면, 로그를 줄이려고 `RUST_LOG=warn`으로 올렸을 때 span이 만들어지지 않아
    /// 추적이 통째로 사라진다: 응답의 `X-Trace-Id`, DB에 남기는 trace ID, 작업에서 접수 요청으로 가는 link까지.
    /// "로그를 얼마나 자세히 볼지"와 "추적을 할지"는 서로 다른 결정이어야 한다.
    ///
    /// 거르는 일은 [`EnvFilter`]에 그대로 맡기고, 그 답을 span에 대해서만 고쳐 준다.
    pub(crate) struct GlobalFilter {
        events: EnvFilter,
    }

    impl GlobalFilter {
        /// `RUST_LOG`가 있으면 그 값으로, 없으면 `default_filter`로 로그를 거른다
        #[cfg(not(feature = "tokio-console"))]
        pub(crate) fn from_env(default_filter: &str) -> Self {
            Self {
                events: super::env_filter(default_filter),
            }
        }

        /// `RUST_LOG`를 보지 않고 `directives`로만 거른다 (테스트용: 실행 환경에 따라 결과가 달라지지 않게)
        #[cfg(feature = "testing")]
        pub(crate) fn exact(directives: &str) -> Self {
            Self {
                events: EnvFilter::new(directives),
            }
        }
    }

    impl<S: Subscriber> Layer<S> for GlobalFilter {
        /// callsite(코드에서 span · 이벤트를 만드는 자리)마다 처음 한 번 불린다.
        /// "항상 켬 / 항상 끔 / 그때그때 물어봄" 중 하나를 답하면 tracing이 그 자리에 기억해 둔다.
        fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
            // 안쪽 필터도 이 자리를 알아야 한다 (span의 필드 값으로 거르는 지시가 있으면 여기서 준비한다)
            let interest = Layer::<S>::register_callsite(&self.events, metadata);
            if always_traced(metadata) {
                Interest::always()
            } else {
                interest
            }
        }

        /// "그때그때 물어봄"이라고 답한 자리에서 span · 이벤트를 만들 때마다 불린다
        fn enabled(&self, metadata: &Metadata<'_>, ctx: Context<'_, S>) -> bool {
            always_traced(metadata) || self.events.enabled(metadata, ctx)
        }

        /// "이보다 자세한 수준은 볼 일이 없다"는 힌트. tracing이 그보다 자세한 것을 필터에 묻지도 않고 건너뛴다.
        /// info span은 항상 받아야 하므로, `RUST_LOG=warn`이어도 info까지는 물어보게 한다.
        fn max_level_hint(&self) -> Option<LevelFilter> {
            self.events
                .max_level_hint()
                .map(|hint| hint.max(LevelFilter::INFO))
        }

        // 아래 다섯은 안쪽 필터에 그대로 넘긴다. `RUST_LOG=[job{job_id=3}]=debug`처럼
        // "이 span 안에서만 자세히"라는 지시를 쓸 때, 필터가 span의 시작과 끝을 따라가야 하기 때문이다.
        fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
            self.events.on_new_span(attrs, id, ctx);
        }

        fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
            self.events.on_record(id, values, ctx);
        }

        fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
            self.events.on_enter(id, ctx);
        }

        fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
            self.events.on_exit(id, ctx);
        }

        fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
            self.events.on_close(id, ctx);
        }
    }

    /// info 이상인 span인가. 수준은 `ERROR < WARN < INFO < DEBUG < TRACE` 순으로 자세해진다.
    fn always_traced(metadata: &Metadata<'_>) -> bool {
        metadata.is_span() && *metadata.level() <= Level::INFO
    }
}

/// OpenTelemetry 로그로 내보내는 layer를 감싸, **로그를 내보내는 과정에서 생긴 로그**는 넘기지 않는다.
///
/// 로그를 내보낼 때 HTTP 클라이언트가 debug 로그를 남긴다 ("연결을 재사용한다" 등). 그것까지 내보내면
/// 내보낼 때마다 로그가 또 생겨 끝없이 되먹임된다. `RUST_LOG=debug`로 띄우면 요청이 하나도 없어도
/// 로그가 초당 여러 줄씩 쌓인다. SDK는 자기 스레드에서 생긴 로그를 스스로 걸러 주지만,
/// HTTP 클라이언트는 별도 스레드에서 일하므로 걸러지지 않는다.
///
/// 콘솔에는 그대로 나온다 (이 layer는 OpenTelemetry 로그로 가는 길에만 있다).
/// 안쪽 layer(로그 브리지)는 이벤트만 쓰므로 `on_event`만 넘긴다.
pub(crate) struct WithoutExporterLogs<L> {
    inner: L,
}

impl<L> WithoutExporterLogs<L> {
    pub(crate) fn new(inner: L) -> Self {
        Self { inner }
    }
}

impl<S: Subscriber, L: Layer<S>> Layer<S> for WithoutExporterLogs<L> {
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if !from_exporter(event.metadata().target()) {
            self.inner.on_event(event, ctx);
        }
    }
}

fn from_exporter(target: &str) -> bool {
    EXPORTER_TARGETS
        .iter()
        .any(|prefix| target.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 내보내는_과정에서_생긴_로그의_대상을_알아본다() {
        for target in [
            "hyper_util::client::legacy::pool",
            "hyper::proto::h1::io",
            "reqwest::connect",
            "h2::codec::framed_write",
            "opentelemetry_sdk",
            "opentelemetry-otlp",
        ] {
            assert!(from_exporter(target), "{target}");
        }
        for target in [
            "booking_server::worker",
            "providers::resilience",
            "telemetry::http",
            "mock_provider::chaos",
            "sqlx::query",
        ] {
            assert!(!from_exporter(target), "{target}");
        }
    }
}
