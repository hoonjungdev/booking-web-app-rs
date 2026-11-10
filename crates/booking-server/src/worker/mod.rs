//! 작업 큐 워커 (docs/02-domain-and-data.md §5)
//!
//! 워커 루프 `concurrency`개를 동시에 돌린다. 루프 하나는 "작업 하나 가져오기 → 처리 → 다음 작업" 을 반복하고,
//! 가져올 작업이 없으면 `poll_interval`만큼 쉰다. 작업을 가져올 때 `FOR UPDATE SKIP LOCKED`를 쓰므로
//! 루프끼리(서버가 여러 대여도) 같은 작업을 동시에 처리하지 않는다.
//!
//! 종료 신호(`CancellationToken`)를 받으면 새 작업을 가져오지 않고, 처리 중이던 작업은 끝까지 마친 뒤 멈춘다.

mod backoff;
mod submit;

use std::{sync::Arc, time::Duration};

use db::{
    PgPool,
    jobs::{self, Job, JobKind},
};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub use backoff::job_backoff;
pub use submit::{Decision, decide};

use crate::registry::ProviderRegistry;

/// 워커 이름. `booking_events.actor`에 남는다.
const ACTOR: &str = "worker";

#[derive(Debug, Clone, Copy)]
pub struct WorkerConfig {
    /// 동시에 처리할 작업 수
    pub concurrency: usize,
    /// 할 일이 없을 때 다시 확인하기까지 쉬는 시간
    pub poll_interval: Duration,
    /// 작업 하나를 쥐고 있을 수 있는 시간. 넘기면 다른 워커가 회수한다.
    pub lease: Duration,
}

/// 워커가 공유하는 의존성 (≈ C#에서 생성자로 주입받는 서비스 묶음)
#[derive(Clone)]
pub struct WorkerContext {
    pub pool: PgPool,
    pub providers: Arc<ProviderRegistry>,
    pub config: WorkerConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Db(#[from] db::DbError),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// 종료 신호가 올 때까지 워커 루프들을 돌린다.
pub async fn run(ctx: WorkerContext, shutdown: CancellationToken) {
    // JoinSet: 여러 태스크를 띄우고 모두 끝나기를 기다리는 묶음 (≈ C#의 `Task.WhenAll`)
    let mut loops = JoinSet::new();
    for index in 0..ctx.config.concurrency {
        loops.spawn(run_loop(ctx.clone(), shutdown.clone(), index));
    }
    tracing::info!(concurrency = ctx.config.concurrency, "worker started");

    while let Some(result) = loops.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "worker loop panicked");
        }
    }
    tracing::info!("worker stopped");
}

async fn run_loop(ctx: WorkerContext, shutdown: CancellationToken, index: usize) {
    while !shutdown.is_cancelled() {
        let idle = match process_next(&ctx).await {
            Ok(processed) => !processed,
            Err(error) => {
                // DB가 잠깐 끊기는 등의 오류. 루프는 멈추지 않고 잠시 쉰 뒤 다시 시도한다.
                tracing::error!(loop_index = index, %error, "작업 처리 중 오류");
                true
            }
        };
        if idle {
            // 둘 중 먼저 끝나는 쪽: 종료 신호가 오면 바로 빠져나가고, 아니면 poll_interval만큼 쉰다
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(ctx.config.poll_interval) => {}
            }
        }
    }
}

/// 실행할 작업 하나를 가져와 처리한다. 처리할 작업이 없었으면 `Ok(false)`.
///
/// 테스트는 루프를 돌리지 않고 이 함수를 직접 불러 한 단계씩 확인한다.
pub async fn process_next(ctx: &WorkerContext) -> Result<bool, WorkerError> {
    let Some(job) = jobs::claim_next(&ctx.pool, ctx.config.lease).await? else {
        return Ok(false);
    };
    process(ctx, job).await?;
    Ok(true)
}

/// `#[tracing::instrument]`: 이 함수 실행 동안 `job` span을 열어, 안에서 남기는 로그에 job_id 등이 함께 찍히게 한다.
#[tracing::instrument(
    name = "job",
    skip_all,
    fields(job_id = job.id, kind = %job.kind, attempt = job.attempts, booking_id = tracing::field::Empty)
)]
async fn process(ctx: &WorkerContext, job: Job) -> Result<(), WorkerError> {
    match job.kind.parse::<JobKind>() {
        Ok(JobKind::SubmitBooking) => submit::handle(ctx, &job).await,
        Err(_) => {
            tracing::error!("알 수 없는 작업 종류라 처리하지 않는다");
            let mut conn = ctx.pool.acquire().await?;
            jobs::mark_dead(&mut conn, job.lease(), "unsupported job kind").await?;
            Ok(())
        }
    }
}
