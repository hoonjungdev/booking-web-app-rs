//! 장애 주입이 가능한 가짜 외부 예약 업체 서버
//!
//! 실행: `cargo run -p mock-provider`
//! 환경 변수:
//! - `MOCK_PROVIDER_ADDR`: 바인드 주소 (기본 `127.0.0.1:9090`)
//! - `RUST_LOG`: 로그 수준 (예: `debug`)
//! - `LOG_FORMAT`: `json`이면 JSON 로그
//! - `OTEL_EXPORTER_OTLP_ENDPOINT`: 추적 · 메트릭 · 로그를 보낼 수집기 주소 (기본 `http://localhost:4318`)
//! - `OTEL_SDK_DISABLED`: `true`면 내보내지 않는다

use std::net::SocketAddr;

use anyhow::Context;
use mock_provider::{AppState, chaos::Preset};
use telemetry::{Export, LogFormat, Settings};

const DEFAULT_ADDR: &str = "127.0.0.1:9090";

/// `#[tokio::main]`은 비동기 런타임을 만들고 그 위에서 `main`을 실행해 준다 (≈ C#의 `async Task Main`).
/// `anyhow::Result`는 "어떤 에러든 담을 수 있는 Result"로, 실행 파일의 최상위에서 쓰기 편하다.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 반환값을 변수에 담아 `main`이 끝날 때까지 들고 있는다. 사라질 때 남은 추적 · 메트릭 · 로그를 마저 내보낸다.
    let _telemetry = telemetry::init(Settings {
        service_name: "mock-provider",
        service_version: env!("CARGO_PKG_VERSION"),
        default_filter: "info,mock_provider=debug,telemetry=debug",
        log_format: LogFormat::from_env(),
        export: Export::Otlp,
    })?;
    telemetry::register_runtime_metrics();

    let addr: SocketAddr = std::env::var("MOCK_PROVIDER_ADDR")
        .unwrap_or_else(|_| DEFAULT_ADDR.to_owned())
        .parse()
        .context("MOCK_PROVIDER_ADDR는 `호스트:포트` 형식이어야 합니다")?;

    let state = AppState::new(&Preset::Healthy.config());
    let app = mock_provider::app(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("{addr}에 바인드할 수 없습니다"))?;
    tracing::info!(%addr, "mock-provider listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("서버 실행 중 오류")?;

    tracing::info!("mock-provider stopped");
    Ok(())
}

/// Ctrl+C 또는 SIGTERM을 기다린다. 신호가 오면 새 요청을 받지 않고 처리 중인 요청을 마무리한 뒤 종료한다.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "Ctrl+C 신호를 받을 수 없습니다");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "SIGTERM 신호를 받을 수 없습니다");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    // 둘 중 먼저 끝나는 쪽을 기다린다 (≈ C#의 Task.WhenAny)
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
