//! 작업 큐 (`jobs`) — docs/02-domain-and-data.md §5
//!
//! 시각 계산(`run_at`, `locked_until`)은 모두 DB의 `now()`로 한다. 서버가 여러 대여도 시계 하나를 기준으로 삼기 위해서다.
//!
//! 작업을 가져가면 `attempts`가 1 늘어나고, 그 값이 이 작업을 쥔 증표([`Lease`]) 역할을 한다.
//! 처리 도중 lease가 만료되어 다른 워커가 같은 작업을 다시 가져가면 `attempts`가 또 늘어나므로,
//! 늦게 끝난 원래 워커의 완료 처리(`WHERE attempts = $n`)는 아무 행도 바꾸지 못한다 (fencing token).
//!
//! 작업을 등록할 때 등록한 쪽의 trace 정보(`trace_context`)를 함께 저장한다. 작업은 나중에 다른 태스크가 처리하므로,
//! 이 값이 "접수 요청의 trace"와 "작업 처리의 trace"를 잇는 유일한 끈이다 (docs/05-observability-and-analytics.md §3).

use std::{str::FromStr, time::Duration};

use chrono::{DateTime, Utc};
use domain::BookingId;
use sqlx::PgConnection;

use crate::{DbError, PgPool};

/// 작업 하나가 최대 몇 번까지 시도되는가 (docs/03-resilience.md §4)
pub const DEFAULT_MAX_ATTEMPTS: i32 = 8;

/// 작업 종류. Phase 6에서 `cancel_booking`, `reconcile_provider`가 추가된다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// 접수한 예약을 업체에 제출한다
    SubmitBooking,
    /// 결과 불명(`unknown`)인 예약을 업체에서 조회해 확인한다 (docs/03-resilience.md §5)
    ResolveUnknown,
}

impl JobKind {
    /// 모든 종류. 종류를 추가하면 여기에도 넣는다 (종류별 지표를 0으로 채울 때 쓴다).
    pub const ALL: [Self; 2] = [Self::SubmitBooking, Self::ResolveUnknown];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SubmitBooking => "submit_booking",
            Self::ResolveUnknown => "resolve_unknown",
        }
    }
}

impl FromStr for JobKind {
    type Err = DbError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "submit_booking" => Ok(Self::SubmitBooking),
            "resolve_unknown" => Ok(Self::ResolveUnknown),
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
    /// 작업 종류별 진행 상황 (예: `resolve_unknown`이 "없음"을 확인한 횟수)
    pub payload: serde_json::Value,
    /// 이 작업을 등록한 요청 · 작업의 trace 문맥 (없을 수 있다)
    pub trace_context: Option<serde_json::Value>,
    /// 실행하기로 한 시각(`run_at`)부터 실제로 가져가기까지 밀린 시간. 워커가 바쁘거나 멈춰 있으면 길어진다.
    pub lag: Duration,
}

/// 작업 종류별 큐 상태 (지표용)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueDepth {
    pub kind: String,
    /// 지금 바로 실행할 수 있는데 아직 가져가지 않은 작업 수
    pub ready: i64,
    /// 더 시도하지 않고 묻어 둔 작업 수 (관리자 확인 대상)
    pub dead: i64,
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
    /// `queued`, `running`, `done`, `dead` (DB의 CHECK 제약이 이 값만 허용한다)
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub run_at: DateTime<Utc>,
    /// 실행 중일 때 이 시각까지 이 작업을 쥐고 있을 수 있다 (lease)
    pub locked_until: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 관리자 작업 목록의 조건. `None`인 조건은 걸지 않는다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobFilter {
    pub status: Option<String>,
    pub kind: Option<String>,
    /// 이 ID보다 먼저 등록된 작업만 (커서 페이지네이션)
    pub before: Option<i64>,
}

/// 작업 종류별 상태 개수 (관리자 화면의 작업 큐)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindCounts {
    pub kind: String,
    /// 지금 실행할 수 있는데 아직 가져가지 않은 작업
    pub ready: i64,
    /// 나중에 실행하기로 미뤄 둔 작업 (재시도 대기 등)
    pub scheduled: i64,
    /// 워커가 처리 중인 작업
    pub running: i64,
    /// 더 시도하지 않고 묻어 둔 작업
    pub dead: i64,
}

/// 작업을 등록한다. 보통 예약 저장과 같은 트랜잭션 안에서 부른다.
///
/// `trace_context`는 등록하는 쪽의 trace 문맥이다 ([`crate::TraceRef::context`]). 없으면 `None`.
pub async fn enqueue(
    conn: &mut PgConnection,
    kind: JobKind,
    booking_id: Option<BookingId>,
    max_attempts: i32,
    trace_context: Option<&serde_json::Value>,
) -> Result<i64, DbError> {
    let empty = serde_json::json!({});
    enqueue_with_payload(conn, kind, booking_id, max_attempts, &empty, trace_context).await
}

/// [`enqueue`]와 같지만 작업의 진행 상황(`payload`)을 처음부터 정해 둔다.
/// 예: 관리자 재시도는 `{"lookup_first": true}`로 "다시 보내기 전에 업체에서 먼저 찾아본다"를 남긴다.
#[tracing::instrument(name = "db.jobs.enqueue", skip_all, fields(kind = kind.as_str()))]
pub async fn enqueue_with_payload(
    conn: &mut PgConnection,
    kind: JobKind,
    booking_id: Option<BookingId>,
    max_attempts: i32,
    payload: &serde_json::Value,
    trace_context: Option<&serde_json::Value>,
) -> Result<i64, DbError> {
    let booking_id = booking_id.map(|id| id.as_uuid());
    let id = sqlx::query_scalar!(
        "INSERT INTO jobs (kind, booking_id, max_attempts, payload, trace_context)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
        kind.as_str(),
        booking_id,
        max_attempts,
        payload,
        trace_context,
    )
    .fetch_one(conn)
    .await?;
    Ok(id)
}

/// 실행할 작업 하나를 가져간다. 없으면 `None`.
///
/// `FOR UPDATE SKIP LOCKED`: 다른 워커가 잠근 행은 기다리지 않고 건너뛴다. 그래서 워커가 여러 개여도 같은 작업을 동시에 집지 않는다.
/// `lease` 동안 끝내지 못하면(워커가 죽는 등) 다른 워커가 다시 가져갈 수 있다.
///
/// 이 함수에는 span을 열지 않는다. 워커가 할 일이 없을 때도 계속 부르는 함수라서, 열면 빈 trace가 끝없이 쌓인다.
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
        RETURNING id, kind, booking_id, attempts, max_attempts, payload, trace_context,
                  EXTRACT(EPOCH FROM (now() - run_at))::float8 AS "lag_secs!"
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
        payload: r.payload,
        trace_context: r.trace_context,
        lag: crate::non_negative_secs(r.lag_secs),
    }))
}

/// 작업을 끝낸다. lease를 잃었으면(다른 워커가 다시 가져감) `false`.
#[tracing::instrument(name = "db.jobs.complete", skip_all)]
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
#[tracing::instrument(name = "db.jobs.retry_later", skip_all)]
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

/// [`retry_later`]와 같지만 작업의 진행 상황(`payload`)도 함께 바꾼다. lease를 잃었으면 `false`.
#[tracing::instrument(name = "db.jobs.retry_later", skip_all)]
pub async fn retry_later_with_payload(
    conn: &mut PgConnection,
    lease: Lease,
    delay: Duration,
    error: &str,
    payload: &serde_json::Value,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE jobs
         SET status = 'queued', run_at = now() + make_interval(secs => $3),
             locked_until = NULL, last_error = $4, payload = $5, updated_at = now()
         WHERE id = $1 AND status = 'running' AND attempts = $2",
        lease.job_id,
        lease.attempt,
        delay.as_secs_f64(),
        error,
        payload,
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 업체를 부르지 않고 `delay` 뒤로 미룬다. lease를 잃었으면 `false`.
///
/// 서킷이 열려 있거나 업체가 쉬라고 해서 호출하지 않은 경우다. 실제로 시도하지 않았으므로
/// 가져갈 때 늘린 `attempts`를 되돌린다 (docs/02-domain-and-data.md §5). 전체 기한은 예약의 `deadline_at`이 막는다.
#[tracing::instrument(name = "db.jobs.defer", skip_all)]
pub async fn defer(
    conn: &mut PgConnection,
    lease: Lease,
    delay: Duration,
    reason: &str,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE jobs
         SET status = 'queued', attempts = attempts - 1, run_at = now() + make_interval(secs => $3),
             locked_until = NULL, last_error = $4, updated_at = now()
         WHERE id = $1 AND status = 'running' AND attempts = $2",
        lease.job_id,
        lease.attempt,
        delay.as_secs_f64(),
        reason,
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 더 시도하지 않는 작업(dead)으로 표시한다. 관리자 화면에서 확인 · 재처리 대상. lease를 잃었으면 `false`.
#[tracing::instrument(name = "db.jobs.mark_dead", skip_all)]
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

/// 작업 종류별로 "바로 실행할 수 있는 작업 수"와 "dead 작업 수"를 센다 (`jobs.ready`, `jobs.dead` 지표).
///
/// 해당하는 작업이 하나도 없는 종류는 결과에 없다 (부르는 쪽이 0으로 채운다).
/// 두 조건은 각각 부분 인덱스(`jobs_ready_idx`, `jobs_dead_idx`)로 찾으므로 끝난 작업이 많이 쌓여도 가볍다.
pub async fn queue_depths(pool: &PgPool) -> Result<Vec<QueueDepth>, DbError> {
    // `count(*) FILTER (WHERE ..)`: 조건에 맞는 행만 센다. 한 번 훑으면서 두 가지를 따로 셀 수 있다.
    let rows = sqlx::query!(
        r#"
        SELECT kind,
               count(*) FILTER (WHERE status = 'queued') AS "ready!",
               count(*) FILTER (WHERE status = 'dead')   AS "dead!"
        FROM jobs
        WHERE (status = 'queued' AND run_at <= now()) OR status = 'dead'
        GROUP BY kind
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| QueueDepth {
            kind: r.kind,
            ready: r.ready,
            dead: r.dead,
        })
        .collect())
}

/// DB 행을 그대로 담는 구조체 ([`JobRecord`]로 바꿔 내보낸다)
struct JobRow {
    id: i64,
    kind: String,
    booking_id: Option<uuid::Uuid>,
    status: String,
    attempts: i32,
    max_attempts: i32,
    run_at: DateTime<Utc>,
    locked_until: Option<DateTime<Utc>>,
    last_error: Option<String>,
    payload: serde_json::Value,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<JobRow> for JobRecord {
    fn from(r: JobRow) -> Self {
        Self {
            id: r.id,
            kind: r.kind,
            booking_id: r.booking_id.map(BookingId::from_uuid),
            status: r.status,
            attempts: r.attempts,
            max_attempts: r.max_attempts,
            run_at: r.run_at,
            locked_until: r.locked_until,
            last_error: r.last_error,
            payload: r.payload,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// 예약에 딸린 작업 목록 (등록 순서대로)
#[tracing::instrument(name = "db.jobs.for_booking", skip_all)]
pub async fn for_booking(pool: &PgPool, booking_id: BookingId) -> Result<Vec<JobRecord>, DbError> {
    let rows = sqlx::query_as!(
        JobRow,
        "SELECT id, kind, booking_id, status, attempts, max_attempts, run_at, locked_until,
                last_error, payload, created_at, updated_at
         FROM jobs WHERE booking_id = $1 ORDER BY id",
        booking_id.as_uuid(),
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(JobRecord::from).collect())
}

/// 작업 하나
#[tracing::instrument(name = "db.jobs.get", skip_all)]
pub async fn get(conn: &mut PgConnection, id: i64) -> Result<Option<JobRecord>, DbError> {
    let row = sqlx::query_as!(
        JobRow,
        "SELECT id, kind, booking_id, status, attempts, max_attempts, run_at, locked_until,
                last_error, payload, created_at, updated_at
         FROM jobs WHERE id = $1",
        id,
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.map(JobRecord::from))
}

/// 작업 목록 (최근에 등록한 것부터, 최대 `limit`건). 관리자 화면용.
///
/// dead 작업만 보는 조회는 부분 인덱스 `jobs_dead_idx (kind, id) WHERE status = 'dead'`를 쓴다.
#[tracing::instrument(name = "db.jobs.list", skip_all)]
pub async fn list(
    pool: &PgPool,
    filter: &JobFilter,
    limit: i64,
) -> Result<Vec<JobRecord>, DbError> {
    let rows = sqlx::query_as!(
        JobRow,
        r#"
        SELECT id, kind, booking_id, status, attempts, max_attempts, run_at, locked_until,
               last_error, payload, created_at, updated_at
        FROM jobs
        WHERE ($1::text IS NULL OR status = $1)
          AND ($2::text IS NULL OR kind = $2)
          AND ($3::int8 IS NULL OR id < $3)
        ORDER BY id DESC
        LIMIT $4
        "#,
        filter.status.as_deref(),
        filter.kind.as_deref(),
        filter.before,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(JobRecord::from).collect())
}

/// 작업 종류별로 대기 · 미룸 · 실행 중 · dead 작업 수를 센다. 끝난 작업(done)은 세지 않는다 (계속 쌓이므로).
///
/// 해당하는 작업이 하나도 없는 종류는 결과에 없다.
#[tracing::instrument(name = "db.jobs.counts", skip_all)]
pub async fn counts(pool: &PgPool) -> Result<Vec<KindCounts>, DbError> {
    let rows = sqlx::query_as!(
        KindCounts,
        r#"
        SELECT kind,
               count(*) FILTER (WHERE status = 'queued' AND run_at <= now()) AS "ready!",
               count(*) FILTER (WHERE status = 'queued' AND run_at > now())  AS "scheduled!",
               count(*) FILTER (WHERE status = 'running')                    AS "running!",
               count(*) FILTER (WHERE status = 'dead')                       AS "dead!"
        FROM jobs
        WHERE status IN ('queued', 'running', 'dead')
        GROUP BY kind
        ORDER BY kind
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// dead 작업을 다시 실행 대기열에 넣는다. dead가 아니면(그사이 누가 먼저 되살림) 아무것도 하지 않고 `false`.
///
/// 같은 행을 되살린다 (새 작업을 만들지 않는다). 그래야 한 예약에 처리 중인 작업이 둘이 되지 않고,
/// dead 작업 수(`jobs.dead` 지표 · 알림)도 바로 줄어든다. 시도 횟수는 처음부터 다시 센다.
/// `kind`와 `payload`는 바꿀 수 있다: 실패한 예약을 되살릴 때는 예약 상태에 맞는 작업(`submit_booking`)으로 바꾼다.
/// `trace_context`에는 되살린 요청의 trace를 넣어, 워커가 그 요청으로 link를 걸게 한다.
#[tracing::instrument(name = "db.jobs.requeue", skip_all)]
pub async fn requeue(
    conn: &mut PgConnection,
    id: i64,
    kind: &str,
    payload: &serde_json::Value,
    trace_context: Option<&serde_json::Value>,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE jobs
         SET status = 'queued', kind = $2, payload = $3, trace_context = $4,
             attempts = 0, run_at = now(), locked_until = NULL, updated_at = now()
         WHERE id = $1 AND status = 'dead'",
        id,
        kind,
        payload,
        trace_context,
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 예약에 대기 중이거나 실행 중인 작업이 있는가
pub async fn has_active_for_booking(
    conn: &mut PgConnection,
    booking_id: BookingId,
) -> Result<bool, DbError> {
    let active = sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM jobs WHERE booking_id = $1 AND status IN ('queued', 'running')
           ) AS "active!""#,
        booking_id.as_uuid(),
    )
    .fetch_one(conn)
    .await?;
    Ok(active)
}

/// 예약의 dead 작업 중 가장 최근 것의 ID
pub async fn latest_dead_for_booking(
    conn: &mut PgConnection,
    booking_id: BookingId,
) -> Result<Option<i64>, DbError> {
    let id = sqlx::query_scalar!(
        "SELECT id FROM jobs WHERE booking_id = $1 AND status = 'dead' ORDER BY id DESC LIMIT 1",
        booking_id.as_uuid(),
    )
    .fetch_optional(conn)
    .await?;
    Ok(id)
}
