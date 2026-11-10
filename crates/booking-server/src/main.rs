//! 예약 API + 관리자 API + 워커 실행 파일
//!
//! ```text
//! booking-server serve [--role all|api|worker]   서버 실행 (기본: API와 워커를 함께)
//! booking-server create-client --name <이름>      API 클라이언트를 만들고 키 발급
//! ```
//! 두 명령 모두 시작할 때 DB 마이그레이션을 적용한다. 설정은 `booking_server::config` 참고.

use std::sync::Arc;

use anyhow::Context;
use booking_server::{
    api::{self, ApiState},
    auth,
    config::Config,
    registry::ProviderRegistry,
    worker::{self, WorkerContext},
};
use clap::{Parser, Subcommand, ValueEnum};
use db::PgPool;
use domain::ClientId;
use telemetry::LogFormat;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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
    telemetry::init(
        "info,booking_server=debug,tower_http=debug,sqlx=warn",
        LogFormat::from_env(),
    )?;

    let cli = Cli::parse();
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

    if matches!(role, Role::All | Role::Worker) {
        let registry = ProviderRegistry::load(&pool)
            .await
            .context("업체 어댑터를 준비할 수 없습니다")?;
        tracing::info!(providers = ?registry.ids(), "provider adapters loaded");
        let ctx = WorkerContext {
            pool: pool.clone(),
            providers: Arc::new(registry),
            config: config.worker(),
        };
        let token = shutdown.clone();
        tasks.spawn(async move {
            worker::run(ctx, token).await;
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
