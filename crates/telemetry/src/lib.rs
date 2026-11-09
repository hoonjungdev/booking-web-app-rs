//! tracing · OpenTelemetry 초기화 (booking-server와 mock-provider가 공유)
//!
//! Phase 1에서는 콘솔 로그만 설정한다. OpenTelemetry 내보내기는 Phase 4에서 추가한다.

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

pub use tracing_subscriber::util::TryInitError;

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

/// 전역 로거를 설치한다. 프로세스 시작 시 한 번만 부른다.
///
/// 로그 수준은 `RUST_LOG` 환경 변수로 정한다 (예: `RUST_LOG=debug`).
/// 없으면 `default_filter`를 쓴다 (예: `"info,mock_provider=debug"`).
pub fn init(default_filter: &str, format: LogFormat) -> Result<(), TryInitError> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    let registry = tracing_subscriber::registry().with(filter);

    match format {
        LogFormat::Compact => registry.with(fmt::layer().compact()).try_init(),
        LogFormat::Json => registry
            .with(fmt::layer().json().flatten_event(true))
            .try_init(),
    }
}
