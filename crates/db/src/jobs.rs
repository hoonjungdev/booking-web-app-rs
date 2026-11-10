//! 작업 큐 (`jobs`) — docs/02-domain-and-data.md §5
//!
//! 시각 계산(`run_at`, `locked_until`)은 모두 DB의 `now()`로 한다. 서버가 여러 대여도 시계 하나를 기준으로 삼기 위해서다.
//!
//! 작업을 가져가면 `attempts`가 1 늘어나고, 그 값이 이 작업을 쥔 증표([`Lease`]) 역할을 한다.
//! 처리 도중 lease가 만료되어 다른 워커가 같은 작업을 다시 가져가면 `attempts`가 또 늘어나므로,
//! 늦게 끝난 원래 워커의 완료 처리(`WHERE attempts = $n`)는 아무 행도 바꾸지 못한다 (fencing token).

use std::{str::FromStr, time::Duration};

use chrono::{DateTime, Utc};
use domain::BookingId;
use sqlx::PgConnection;

use crate::{DbError, PgPool};

/// 작업 하나가 최대 몇 번까지 시도되는가 (docs/03-resilience.md §4)
pub const DEFAULT_MAX_ATTEMPTS: i32 = 8;

/// 작업 종류. Phase 3 이후 `resolve_unknown`, `cancel_booking` 등이 추가된다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    SubmitBooking,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SubmitBooking => "submit_booking",
        }
    }
}

impl FromStr for JobKind {
    type Err = DbError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "submit_booking" => Ok(Self::SubmitBooking),
            other => Err(DbError::corrupt(format!("알 수 없는 작업 종류: {other}"))),
        }
    }
}

/// 워커가 가져간 작업
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: i64,
    /// DB에 저장된 문자열 그대로. 이 코드가 모르는 종류일 수 있어서 워커가 [`JobKind`]로 바꿔 확인한다.
    pub kind: String,
    pub booking_id: Option<BookingId>,
    /// 이번이 몇 번째 시도인가 (가져갈 때 1 늘어난 값)
    pub attempts: i32,
    pub max_attempts: i32,
}

impl Job {
    pub fn lease(&self) -> Lease {
        Lease {
            job_id: self.id,
            attempt: self.attempts,
        }
    }
}

/// 작업을 쥐고 있다는 증표. 완료 · 재예약 · 포기 처리에 함께 넘긴다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    pub job_id: i64,
    pub attempt: i32,
}

/// 작업 상태 조회 결과 (테스트 · 관리 화면용)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub id: i64,
    pub kind: String,
    pub booking_id: Option<BookingId>,
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub run_at: DateTime<Utc>,
    pub last_error: Option<String>,
}

/// 작업을 등록한다. 보통 예약 저장과 같은 트랜잭션 안에서 부른다.
pub async fn enqueue(
    conn: &mut PgConnection,
    kind: JobKind,
    booking_id: Option<BookingId>,
    max_attempts: i32,
) -> Result<i64, DbError> {
    let booking_id = booking_id.map(|id| id.as_uuid());
    let id = sqlx::query_scalar!(
        "INSERT INTO jobs (kind, booking_id, max_attempts) VALUES ($1, $2, $3) RETURNING id",
        kind.as_str(),
        booking_id,
        max_attempts,
    )
    .fetch_one(conn)
    .await?;
    Ok(id)
}

/// 실행할 작업 하나를 가져간다. 없으면 `None`.
///
/// `FOR UPDATE SKIP LOCKED`: 다른 워커가 잠근 행은 기다리지 않고 건너뛴다. 그래서 워커가 여러 개여도 같은 작업을 동시에 집지 않는다.
/// `lease` 동안 끝내지 못하면(워커가 죽는 등) 다른 워커가 다시 가져갈 수 있다.
pub async fn claim_next(pool: &PgPool, lease: Duration) -> Result<Option<Job>, DbError> {
    let row = sqlx::query!(
        r#"
        UPDATE jobs
        SET status = 'running',
            attempts = attempts + 1,
            locked_until = now() + make_interval(secs => $1),
            updated_at = now()
        WHERE id = (
            SELECT id FROM jobs
            WHERE (status = 'queued' AND run_at <= now())
               OR (status = 'running' AND locked_until < now())
            ORDER BY run_at
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, kind, booking_id, attempts, max_attempts
        "#,
        lease.as_secs_f64(),
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| Job {
        id: r.id,
        kind: r.kind,
        booking_id: r.booking_id.map(BookingId::from_uuid),
        attempts: r.attempts,
        max_attempts: r.max_attempts,
    }))
}

/// 작업을 끝낸다. lease를 잃었으면(다른 워커가 다시 가져감) `false`.
pub async fn complete(conn: &mut PgConnection, lease: Lease) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE jobs SET status = 'done', locked_until = NULL, last_error = NULL, updated_at = now()
         WHERE id = $1 AND status = 'running' AND attempts = $2",
        lease.job_id,
        lease.attempt,
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// `delay` 뒤에 다시 실행하도록 되돌려 놓는다. lease를 잃었으면 `false`.
pub async fn retry_later(
    conn: &mut PgConnection,
    lease: Lease,
    delay: Duration,
    error: &str,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE jobs
         SET status = 'queued', run_at = now() + make_interval(secs => $3),
             locked_until = NULL, last_error = $4, updated_at = now()
         WHERE id = $1 AND status = 'running' AND attempts = $2",
        lease.job_id,
        lease.attempt,
        delay.as_secs_f64(),
        error,
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 더 시도하지 않는 작업(dead)으로 표시한다. 관리자 화면에서 확인 · 재처리 대상. lease를 잃었으면 `false`.
pub async fn mark_dead(
    conn: &mut PgConnection,
    lease: Lease,
    error: &str,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE jobs SET status = 'dead', locked_until = NULL, last_error = $3, updated_at = now()
         WHERE id = $1 AND status = 'running' AND attempts = $2",
        lease.job_id,
        lease.attempt,
        error,
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 예약에 딸린 작업 목록 (등록 순서대로)
pub async fn for_booking(pool: &PgPool, booking_id: BookingId) -> Result<Vec<JobRecord>, DbError> {
    let rows = sqlx::query!(
        "SELECT id, kind, booking_id, status, attempts, max_attempts, run_at, last_error
         FROM jobs WHERE booking_id = $1 ORDER BY id",
        booking_id.as_uuid(),
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| JobRecord {
            id: r.id,
            kind: r.kind,
            booking_id: r.booking_id.map(BookingId::from_uuid),
            status: r.status,
            attempts: r.attempts,
            max_attempts: r.max_attempts,
            run_at: r.run_at,
            last_error: r.last_error,
        })
        .collect())
}
