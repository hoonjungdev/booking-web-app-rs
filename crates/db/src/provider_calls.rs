//! 외부 업체 호출 기록 (`provider_calls`)
//!
//! 쓰기는 booking-server의 기록 태스크가 모아서 [`insert_batch`]로 한 번에 넣는다.

use chrono::{DateTime, Utc};
use domain::BookingId;

use crate::{DbError, PgPool};

/// 저장할 호출 기록 한 건
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProviderCall {
    pub provider_id: String,
    pub operation: String,
    pub booking_id: Option<BookingId>,
    pub job_id: Option<i64>,
    pub attempt: i32,
    pub started_at: DateTime<Utc>,
    pub duration_ms: i32,
    pub outcome: String,
    pub http_status: Option<i32>,
    pub rate_limit_remaining: Option<i32>,
    pub retry_after_ms: Option<i32>,
    pub error_detail: Option<String>,
    pub trace_id: Option<String>,
}

/// 저장된 호출 기록
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCallRecord {
    pub id: i64,
    pub provider_id: String,
    pub operation: String,
    pub booking_id: Option<BookingId>,
    pub job_id: Option<i64>,
    pub attempt: i32,
    pub started_at: DateTime<Utc>,
    pub duration_ms: i32,
    pub outcome: String,
    pub http_status: Option<i32>,
    pub rate_limit_remaining: Option<i32>,
    pub retry_after_ms: Option<i32>,
    pub error_detail: Option<String>,
    /// 이 호출이 속한 trace. Grafana에서 그 호출의 앞뒤(재시도, 업체 내부 처리)를 볼 수 있다.
    pub trace_id: Option<String>,
}

/// 여러 건을 INSERT 한 번으로 넣는다. 넣은 행 수를 돌려준다.
///
/// 열마다 값을 배열 하나로 모아 보내고, PostgreSQL의 `UNNEST`가 배열들을 다시 행으로 펼친다.
/// 그래서 건수와 상관없이 SQL 문장이 하나라서 `query!`로 컴파일 시점에 검사할 수 있다.
///
/// 값이 없는 칸은 배열 안에 `NULL`로 넣는다(`Vec<Option<T>>`). `query!`는 배열 매개변수를 `&[T]`로 기대해서
/// `Option` 원소를 거절하므로, 그런 열은 `값 as &[Option<T>]`로 타입을 직접 알려 준다 (sqlx의 매개변수 타입 지정 문법).
pub async fn insert_batch(pool: &PgPool, calls: &[NewProviderCall]) -> Result<u64, DbError> {
    if calls.is_empty() {
        return Ok(0);
    }
    // 반복자에서 열 하나씩 뽑아 Vec으로 모은다 (≈ LINQ의 `calls.Select(c => c.ProviderId).ToList()`)
    let provider_ids: Vec<String> = calls.iter().map(|c| c.provider_id.clone()).collect();
    let operations: Vec<String> = calls.iter().map(|c| c.operation.clone()).collect();
    let booking_ids: Vec<Option<uuid::Uuid>> = calls
        .iter()
        .map(|c| c.booking_id.map(|id| id.as_uuid()))
        .collect();
    let job_ids: Vec<Option<i64>> = calls.iter().map(|c| c.job_id).collect();
    let attempts: Vec<i32> = calls.iter().map(|c| c.attempt).collect();
    let started_ats: Vec<DateTime<Utc>> = calls.iter().map(|c| c.started_at).collect();
    let durations: Vec<i32> = calls.iter().map(|c| c.duration_ms).collect();
    let outcomes: Vec<String> = calls.iter().map(|c| c.outcome.clone()).collect();
    let http_statuses: Vec<Option<i32>> = calls.iter().map(|c| c.http_status).collect();
    let remainings: Vec<Option<i32>> = calls.iter().map(|c| c.rate_limit_remaining).collect();
    let retry_afters: Vec<Option<i32>> = calls.iter().map(|c| c.retry_after_ms).collect();
    let details: Vec<Option<String>> = calls.iter().map(|c| c.error_detail.clone()).collect();
    let trace_ids: Vec<Option<String>> = calls.iter().map(|c| c.trace_id.clone()).collect();

    let result = sqlx::query!(
        r#"
        INSERT INTO provider_calls (provider_id, operation, booking_id, job_id, attempt, started_at,
                                    duration_ms, outcome, http_status, rate_limit_remaining,
                                    retry_after_ms, error_detail, trace_id)
        SELECT * FROM UNNEST($1::text[], $2::text[], $3::uuid[], $4::int8[], $5::int4[],
                             $6::timestamptz[], $7::int4[], $8::text[], $9::int4[], $10::int4[],
                             $11::int4[], $12::text[], $13::text[])
        "#,
        &provider_ids,
        &operations,
        &booking_ids as &[Option<uuid::Uuid>],
        &job_ids as &[Option<i64>],
        &attempts,
        &started_ats,
        &durations,
        &outcomes,
        &http_statuses as &[Option<i32>],
        &remainings as &[Option<i32>],
        &retry_afters as &[Option<i32>],
        &details as &[Option<String>],
        &trace_ids as &[Option<String>],
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// 예약 하나에 대한 호출 기록 (오래된 것부터). 관리자 화면의 예약 상세 · 테스트용.
#[tracing::instrument(name = "db.provider_calls.for_booking", skip_all)]
pub async fn for_booking(
    pool: &PgPool,
    booking_id: BookingId,
) -> Result<Vec<ProviderCallRecord>, DbError> {
    let rows = sqlx::query!(
        "SELECT id, provider_id, operation, booking_id, job_id, attempt, started_at, duration_ms,
                outcome, http_status, rate_limit_remaining, retry_after_ms, error_detail, trace_id
         FROM provider_calls WHERE booking_id = $1 ORDER BY id",
        booking_id.as_uuid(),
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| ProviderCallRecord {
            id: r.id,
            provider_id: r.provider_id,
            operation: r.operation,
            booking_id: r.booking_id.map(BookingId::from_uuid),
            job_id: r.job_id,
            attempt: r.attempt,
            started_at: r.started_at,
            duration_ms: r.duration_ms,
            outcome: r.outcome,
            http_status: r.http_status,
            rate_limit_remaining: r.rate_limit_remaining,
            retry_after_ms: r.retry_after_ms,
            error_detail: r.error_detail,
            trace_id: r.trace_id,
        })
        .collect())
}

// ───────────────────────── 통계 (docs/05-observability-and-analytics.md §7) ─────────────────────────
//
// 관문에서 막혀 업체를 부르지 않은 시도(`circuit_open`, `throttled`)는 0ms에 가까우므로 지연 분위수에서 뺀다.
// 넣으면 p95가 실제보다 좋아 보인다. 호출 수 · 결과별 수에는 그대로 넣어 "얼마나 막혔는지"를 함께 본다.
//
// 구간은 `date_bin(간격, 시각, 기준 시각)`으로 나눈다: 기준 시각부터 간격 단위로 잘라 그 시각이 속한 구간의 시작을 돌려준다.
// 기준 시각을 UTC로 못 박아 둔다 (`timestamptz '2000-01-01'`처럼 오프셋 없이 쓰면 DB 세션의 시간대를 따른다).

/// 시계열 한 구간의 집계
#[derive(Debug, Clone, PartialEq)]
pub struct StatsBucket {
    /// 구간의 시작 시각
    pub bucket: DateTime<Utc>,
    /// 모든 시도 (관문에서 막힌 것 포함)
    pub calls: i64,
    /// 같은 호출 안에서 다시 시도한 횟수 (`attempt > 1`)
    pub retries: i64,
    /// 실제로 업체를 부른 시도의 지연 분위수 (밀리초). 그런 시도가 없으면 `None`
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    /// 업체가 알려 준 남은 호출 수 중 가장 작은 값
    pub min_rate_limit_remaining: Option<i32>,
}

/// 구간 · 결과별 시도 수
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeCount {
    pub bucket: DateTime<Utc>,
    pub outcome: String,
    pub count: i64,
}

/// 업체 하나의 기간 요약 (대시보드 카드)
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCallSummary {
    pub provider_id: String,
    /// 모든 시도 (관문에서 막힌 것 포함)
    pub attempts: i64,
    /// 실제로 업체를 부른 시도
    pub called: i64,
    /// 업체가 정상으로 답한 시도 (`success` + 업무적 거절 `rejected`)
    pub answered: i64,
    /// 업체가 호출 한도 초과를 알린 시도 (429 · 503 + Retry-After)
    pub rate_limited: i64,
    /// 관문(서킷 · rate limiter · bulkhead · pause)에서 막혀 부르지 않은 시도
    pub blocked: i64,
    pub retries: i64,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
}

/// 업체 하나의 `[from, to)` 기간을 `bucket` 간격으로 나눠 구간마다 집계한다. 호출이 없는 구간은 결과에 없다.
#[tracing::instrument(name = "db.provider_calls.timeseries", skip_all)]
pub async fn timeseries(
    pool: &PgPool,
    provider_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    bucket: std::time::Duration,
) -> Result<Vec<StatsBucket>, DbError> {
    let rows = sqlx::query_as!(
        StatsBucket,
        r#"
        SELECT
          date_bin(make_interval(secs => $4), started_at, '2000-01-01T00:00:00Z'::timestamptz) AS "bucket!",
          count(*)                                                       AS "calls!",
          count(*) FILTER (WHERE attempt > 1)                            AS "retries!",
          percentile_cont(0.50) WITHIN GROUP (ORDER BY duration_ms)
              FILTER (WHERE outcome NOT IN ('circuit_open', 'throttled')) AS p50_ms,
          percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms)
              FILTER (WHERE outcome NOT IN ('circuit_open', 'throttled')) AS p95_ms,
          percentile_cont(0.99) WITHIN GROUP (ORDER BY duration_ms)
              FILTER (WHERE outcome NOT IN ('circuit_open', 'throttled')) AS p99_ms,
          min(rate_limit_remaining)                                      AS min_rate_limit_remaining
        FROM provider_calls
        WHERE provider_id = $1 AND started_at >= $2 AND started_at < $3
        GROUP BY 1
        ORDER BY 1
        "#,
        provider_id,
        from,
        to,
        bucket.as_secs_f64(),
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// [`timeseries`]와 같은 구간에서 결과(`outcome`)별 시도 수
#[tracing::instrument(name = "db.provider_calls.outcome_counts", skip_all)]
pub async fn outcome_counts(
    pool: &PgPool,
    provider_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    bucket: std::time::Duration,
) -> Result<Vec<OutcomeCount>, DbError> {
    let rows = sqlx::query_as!(
        OutcomeCount,
        r#"
        SELECT
          date_bin(make_interval(secs => $4), started_at, '2000-01-01T00:00:00Z'::timestamptz) AS "bucket!",
          outcome,
          count(*) AS "count!"
        FROM provider_calls
        WHERE provider_id = $1 AND started_at >= $2 AND started_at < $3
        GROUP BY 1, 2
        ORDER BY 1, 2
        "#,
        provider_id,
        from,
        to,
        bucket.as_secs_f64(),
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// 업체마다 `since` 이후의 호출을 요약한다. 호출이 없는 업체는 결과에 없다.
///
/// 업체 ID 목록을 받는 이유: `provider_calls_time_idx (provider_id, started_at)` 인덱스는 업체 ID부터 정해져야
/// 쓸 수 있다. 업체 조건 없이 시각으로만 찾으면 테이블 전체를 훑는다.
#[tracing::instrument(name = "db.provider_calls.summary", skip_all)]
pub async fn summary(
    pool: &PgPool,
    provider_ids: &[String],
    since: DateTime<Utc>,
) -> Result<Vec<ProviderCallSummary>, DbError> {
    let rows = sqlx::query_as!(
        ProviderCallSummary,
        r#"
        SELECT
          provider_id,
          count(*)                                                           AS "attempts!",
          count(*) FILTER (WHERE outcome NOT IN ('circuit_open', 'throttled')) AS "called!",
          count(*) FILTER (WHERE outcome IN ('success', 'rejected'))         AS "answered!",
          count(*) FILTER (WHERE outcome = 'rate_limited')                   AS "rate_limited!",
          count(*) FILTER (WHERE outcome IN ('circuit_open', 'throttled'))   AS "blocked!",
          count(*) FILTER (WHERE attempt > 1)                                AS "retries!",
          percentile_cont(0.50) WITHIN GROUP (ORDER BY duration_ms)
              FILTER (WHERE outcome NOT IN ('circuit_open', 'throttled'))     AS p50_ms,
          percentile_cont(0.95) WITHIN GROUP (ORDER BY duration_ms)
              FILTER (WHERE outcome NOT IN ('circuit_open', 'throttled'))     AS p95_ms,
          percentile_cont(0.99) WITHIN GROUP (ORDER BY duration_ms)
              FILTER (WHERE outcome NOT IN ('circuit_open', 'throttled'))     AS p99_ms
        FROM provider_calls
        WHERE provider_id = ANY($1) AND started_at >= $2
        GROUP BY provider_id
        ORDER BY provider_id
        "#,
        provider_ids,
        since,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
