//! 예약 API + 관리자 API + 워커 실행 파일
//!
//! ```text
//! booking-server serve [--role all|api|worker]   서버 실행 (기본: API와 워커를 함께)
//! booking-server create-client --name <이름>      API 클라이언트를 만들고 키 발급
//! ```
//! 두 명령 모두 시작할 때 DB 마이그레이션을 적용한다. 설정은 `booking_server::config` 참고.
//!
//! `serve`는 추적 · 메트릭 · 로그를 OpenTelemetry(OTLP)로 내보낸다 (docs/05-observability-and-analytics.md).
//! 받는 곳은 `OTEL_EXPORTER_OTLP_ENDPOINT`(기본 `http://localhost:4318`)이고, `OTEL_SDK_DISABLED=true`로 끌 수 있다.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use booking_server::{
    DEFAULT_LOG_FILTER,
    api::{self, ApiState},
    auth,
    config::Config,
    metrics, recorder,
    registry::ProviderRegistry,
    worker::{self, WorkerContext},
};
use clap::{Parser, Subcommand, ValueEnum};
use db::PgPool;
use domain::ClientId;
use telemetry::{Export, LogFormat, Settings};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 워커 lease에 업체 호출 시간 말고도 더 남겨 둘 여유 (작업 가져오기 · 결과 저장 같은 DB 작업)
const LEASE_MARGIN: Duration = Duration::from_secs(5);

/// `#[derive(Parser)]`: 구조체 정의만으로 명령줄 인자 해석과 `--help`를 만들어 준다 (≈ System.CommandLine)
#[derive(Parser)]
#[command(
    name = "booking-server",
    version,
    about = "외부 예약 업체 연동 게이트웨이"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 서버를 실행한다
    Serve {
        /// 실행할 역할
        #[arg(long, value_enum, default_value_t = Role::All)]
        role: Role,
    },
    /// 예약 API를 호출할 클라이언트를 만들고 API 키를 발급한다
    CreateClient {
        /// 클라이언트 이름 (예: mobile-app)
        #[arg(long)]
        name: String,
    },
}

/// 한 바이너리로 API와 워커를 함께 또는 따로 띄운다 (docs/decisions.md D-04)
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Role {
    All,
    Api,
    Worker,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // .env 파일이 있으면 환경 변수로 읽어 둔다. 없어도 괜찮다 (`.ok()`로 결과를 버림).
    dotenvy::dotenv().ok();
    let cli = Cli::parse();

    // 반환값을 `_telemetry`에 담아 `main`이 끝날 때까지 들고 있는다. 이 값이 사라질 때(함수가 끝나거나 `?`로 빠져나갈 때)
    // 아직 보내지 않은 추적 · 메트릭 · 로그를 마저 내보낸다. `let _ = ...`로 받으면 그 줄에서 바로 사라져 버린다.
    let _telemetry = telemetry::init(Settings {
        service_name: "booking-server",
        service_version: env!("CARGO_PKG_VERSION"),
        default_filter: DEFAULT_LOG_FILTER,
        log_format: LogFormat::from_env(),
        // 서버만 내보낸다. `create-client`처럼 금방 끝나는 명령은 콘솔 로그면 충분하다.
        export: match cli.command {
            Command::Serve { .. } => Export::Otlp,
            Command::CreateClient { .. } => Export::Off,
        },
    })?;

    let config =
        Config::load().context("설정을 읽을 수 없습니다 (DATABASE_URL이 있는지 확인하세요)")?;
    let pool = db::connect(&config.database_url, config.db_max_connections)
        .await
        .context("DB에 연결할 수 없습니다")?;
    db::MIGRATOR
        .run(&pool)
        .await
        .context("DB 마이그레이션 실패")?;

    match cli.command {
        Command::Serve { role } => serve(&config, pool, role).await,
        Command::CreateClient { name } => create_client(&pool, &name).await,
    }
}

async fn serve(config: &Config, pool: PgPool, role: Role) -> anyhow::Result<()> {
    // 종료 신호를 여러 태스크에 전달하는 토큰 (≈ C#의 CancellationTokenSource)
    let shutdown = CancellationToken::new();
    let mut tasks: JoinSet<anyhow::Result<()>> = JoinSet::new();
    telemetry::register_runtime_metrics();

    if matches!(role, Role::All | Role::Worker) {
        // 업체 호출 기록 채널. 보내는 쪽은 업체 파이프라인마다 나눠 주고, 받는 쪽은 저장 태스크가 쓴다.
        let (call_recorder, calls) = providers::call_recorder(recorder::CHANNEL_CAPACITY);
        let registry = ProviderRegistry::load(&pool, call_recorder.clone())
            .await
            .context("업체 어댑터를 준비할 수 없습니다")?;
        tracing::info!(providers = ?registry.ids(), "provider adapters loaded");

        // lease가 업체 호출 한 번보다 짧으면, 호출 도중 lease가 끝나 다른 워커가 같은 작업을 다시 가져간다
        // (멱등키 없는 업체라면 이중 예약 위험). 이런 설정 실수는 시작할 때 막는다.
        let worker_config = config.worker();
        let needed = registry.longest_call() + LEASE_MARGIN;
        anyhow::ensure!(
            worker_config.lease >= needed,
            "BOOKING_WORKER_LEASE_SECS({}초)가 너무 짧습니다. 업체 호출 한 번이 끝나고 결과를 저장할 시간을 포함해 {}초 이상이어야 합니다",
            worker_config.lease.as_secs(),
            needed.as_secs_f64().ceil()
        );

        // 워커가 멈추고 업체 파이프라인이 사라지면 채널이 닫히고, 남은 기록을 저장한 뒤 이 태스크도 끝난다
        let recorder_pool = pool.clone();
        tasks.spawn(async move {
            recorder::run(recorder_pool, calls).await;
            Ok(())
        });

        let registry = Arc::new(registry);
        // 서킷 상태 · pause · 동시 실행 수를 지표로 내보낸다 (지표를 내보낼 때마다 현재 값을 읽어 간다)
        metrics::register_provider_gauges(&registry);

        // 대기 · dead 작업 수처럼 DB에 물어야 아는 지표를 주기적으로 갱신한다
        let (gauge_pool, token) = (pool.clone(), shutdown.clone());
        tasks.spawn(async move {
            metrics::run_queue_gauges(gauge_pool, token).await;
            Ok(())
        });

        let ctx = WorkerContext {
            pool: pool.clone(),
            providers: registry,
            config: worker_config,
        };
        let token = shutdown.clone();
        tasks.spawn(async move {
            worker::run(ctx, token).await;
            // 기록 채널의 보내는 쪽 하나를 워커가 끝날 때까지 쥐고 있다가 여기서 놓는다.
            // 사용 중인 업체가 하나도 없으면 다른 보내는 쪽이 없어서, 이것이 없으면 채널이 바로 닫히고
            // 기록 태스크가 먼저 끝나 버린다 (그러면 아래 select!가 "태스크 하나가 끝남"으로 보고 서버 전체를 멈춘다).
            drop(call_recorder);
            Ok(())
        });
    }

    if matches!(role, Role::All | Role::Api) {
        let listener = tokio::net::TcpListener::bind(config.listen_addr)
            .await
            .with_context(|| format!("{}에 바인드할 수 없습니다", config.listen_addr))?;
        tracing::info!(addr = %config.listen_addr, "booking-server API listening");
        let app = api::router(ApiState { pool: pool.clone() });
        let token = shutdown.clone();
        tasks.spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(token.cancelled_owned())
                .await
                .context("API 서버 오류")
        });
    }

    // 종료 신호가 오거나, 태스크 하나가 먼저 끝나면(오류 등) 나머지도 정리한다
    let first_exit = tokio::select! {
        () = shutdown_signal() => None,
        result = tasks.join_next() => result,
    };
    shutdown.cancel();
    if let Some(result) = first_exit {
        result.context("태스크가 비정상 종료됨")??;
    }
    while let Some(result) = tasks.join_next().await {
        result.context("태스크가 비정상 종료됨")??;
    }

    pool.close().await;
    tracing::info!("booking-server stopped");
    Ok(())
}

// 이 명령의 목적은 발급한 API 키를 사람에게 보여 주는 것이라 표준 출력을 쓴다 (로그가 아님)
#[allow(clippy::print_stdout)]
async fn create_client(pool: &PgPool, name: &str) -> anyhow::Result<()> {
    let key = auth::generate_api_key();
    let client = db::clients::create(
        pool,
        ClientId::from_uuid(Uuid::now_v7()),
        name,
        &auth::hash_api_key(&key),
    )
    .await
    .context("클라이언트를 만들 수 없습니다")?;

    println!("클라이언트를 만들었습니다: {} ({})", client.name, client.id);
    println!("API 키 (다시 볼 수 없으니 지금 저장하세요): {key}");
    Ok(())
}

/// Ctrl+C 또는 SIGTERM을 기다린다
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

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received, finishing in-flight work");
}
