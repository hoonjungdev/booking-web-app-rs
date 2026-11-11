//! 관리자: 업체 호출 통계 (docs/05-observability-and-analytics.md §7)
//!
//! 원천은 호출 기록(`provider_calls`, 시도 1번 = 1행)이다. 메트릭(Prometheus)의 p95는 히스토그램 버킷으로 보간한 근사값이지만,
//! 여기서는 `percentile_cont`로 정확한 분위수를 낸다. 관문에서 막혀 부르지 않은 시도(`circuit_open`, `throttled`)는
//! 지연 분위수 · 성공률에서 뺀다 (0ms에 가까워서 넣으면 실제보다 좋아 보인다).

use std::{collections::BTreeMap, time::Duration};

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use chrono::{DateTime, TimeDelta, Utc};
use domain::ProviderId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::Latency;
use crate::api::{ApiError, ApiState, Problem, docs::tags};

/// 시계열 한 번에 돌려줄 최대 구간 수 (화면에 그릴 점 수). 넘으면 간격을 넓혀 달라고 한다.
const MAX_POINTS: i64 = 1_000;

/// 시계열 구간 간격
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum StatsInterval {
    #[serde(rename = "10s")]
    TenSeconds,
    #[serde(rename = "1m")]
    OneMinute,
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "15m")]
    FifteenMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

impl StatsInterval {
    fn seconds(self) -> i64 {
        match self {
            Self::TenSeconds => 10,
            Self::OneMinute => 60,
            Self::FiveMinutes => 300,
            Self::FifteenMinutes => 900,
            Self::OneHour => 3_600,
        }
    }
}

/// 요약 기간
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum SummaryWindow {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "15m")]
    FifteenMinutes,
    #[serde(rename = "1h")]
    OneHour,
    #[serde(rename = "6h")]
    SixHours,
    #[serde(rename = "24h")]
    OneDay,
}

impl SummaryWindow {
    fn duration(self) -> TimeDelta {
        match self {
            Self::FiveMinutes => TimeDelta::minutes(5),
            Self::FifteenMinutes => TimeDelta::minutes(15),
            Self::OneHour => TimeDelta::hours(1),
            Self::SixHours => TimeDelta::hours(6),
            Self::OneDay => TimeDelta::hours(24),
        }
    }
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TimeseriesQuery {
    /// 시작 시각 (포함, RFC 3339). 기본: 끝 시각 1시간 전
    from: Option<DateTime<Utc>>,
    /// 끝 시각 (미포함, RFC 3339). 기본: 지금
    to: Option<DateTime<Utc>>,
    /// 구간 간격 (기본 `1m`)
    interval: Option<StatsInterval>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SummaryQuery {
    /// 지금부터 거슬러 올라갈 기간 (기본 `1h`)
    window: Option<SummaryWindow>,
}

/// 업체 하나의 구간별 호출 통계
#[derive(Debug, Serialize, ToSchema)]
pub struct Timeseries {
    pub provider: ProviderId,
    pub interval: StatsInterval,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// 구간마다 하나 (호출이 없는 구간도 0으로 들어 있다)
    pub points: Vec<TimeseriesPoint>,
}

/// 구간 하나
#[derive(Debug, Serialize, ToSchema)]
pub struct TimeseriesPoint {
    /// 구간의 시작 시각
    pub bucket: DateTime<Utc>,
    /// 모든 시도 수 (관문에서 막힌 것 포함)
    pub calls: i64,
    /// 결과(`outcome`)별 시도 수
    pub by_outcome: BTreeMap<String, i64>,
    /// 실제로 업체를 부른 시도의 지연
    pub latency_ms: Latency,
    /// 같은 호출 안에서 다시 시도한 횟수
    pub retries: i64,
    /// 업체가 알려 준 남은 호출 수 중 가장 작은 값
    pub min_rate_limit_remaining: Option<i32>,
}

/// 업체별 요약 (대시보드 카드)
#[derive(Debug, Serialize, ToSchema)]
pub struct StatsSummary {
    pub window: SummaryWindow,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// 모든 업체 (호출이 없는 업체는 0)
    pub providers: Vec<ProviderStats>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderStats {
    pub provider: ProviderId,
    /// 모든 시도 (관문에서 막힌 것 포함)
    pub attempts: i64,
    /// 실제로 업체를 부른 시도
    pub called: i64,
    /// 업체가 정상으로 답한 시도 (`success` + 업무적 거절 `rejected`)
    pub answered: i64,
    /// 업체가 호출 한도 초과를 알린 시도 (429 등)
    pub rate_limited: i64,
    /// 관문(서킷 · rate limiter · bulkhead · pause)에서 막혀 부르지 않은 시도
    pub blocked: i64,
    pub retries: i64,
    /// 성공률 = 정상 응답 ÷ 실제로 부른 시도 (0~1). 부른 적이 없으면 `null`
    pub success_rate: Option<f64>,
    pub latency_ms: Latency,
}

/// 업체 하나의 구간별 호출 수 · 결과 분포 · p50/p95/p99
#[utoipa::path(
    get,
    path = "/stats/providers/{id}/timeseries",
    operation_id = "admin_provider_timeseries",
    tag = tags::ADMIN_STATS,
    params(("id" = String, Path, description = "업체 ID"), TimeseriesQuery),
    responses(
        (status = 200, description = "구간별 통계", body = Timeseries),
        (status = 400, description = "기간 · 간격 오류 (구간이 너무 많음 등)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "없는 업체", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.stats.timeseries", skip_all, fields(provider = %id))]
pub async fn provider_timeseries(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    query: Result<Query<TimeseriesQuery>, QueryRejection>,
) -> Result<Json<Timeseries>, ApiError> {
    let Query(query) = query?;
    let provider = existing_provider(&state, &id).await?;
    let interval = query.interval.unwrap_or(StatsInterval::OneMinute);
    let to = query.to.unwrap_or_else(Utc::now);
    let from = query.from.unwrap_or(to - TimeDelta::hours(1));
    if from >= to {
        return Err(ApiError::InvalidRequest(
            "from은 to보다 앞이어야 합니다".into(),
        ));
    }
    let step = interval.seconds();
    let first = align(from, step);
    // 올림 나눗셈: 마지막 구간이 `to`를 넘어가더라도 그 구간까지 센다 (`to > first`라서 양수다)
    let points = ((to - first).num_seconds() + step - 1) / step;
    if points > MAX_POINTS {
        return Err(ApiError::InvalidRequest(format!(
            "구간이 너무 많습니다 ({points}개, 최대 {MAX_POINTS}개). 기간을 줄이거나 간격을 넓히세요"
        )));
    }

    // 구간 경계에 맞춘 시각부터 조회해야 첫 구간이 온전히 담긴다
    let bucket = Duration::from_secs(step.unsigned_abs());
    let rows =
        db::provider_calls::timeseries(&state.pool, provider.as_str(), first, to, bucket).await?;
    let outcomes =
        db::provider_calls::outcome_counts(&state.pool, provider.as_str(), first, to, bucket)
            .await?;

    // 호출이 없는 구간은 DB 결과에 없다. 차트가 끊기지 않도록 모든 구간을 0으로 만들어 두고 채운다.
    let mut points: BTreeMap<DateTime<Utc>, TimeseriesPoint> = (0..points)
        .map(|n| first + TimeDelta::seconds(n * step))
        .map(|bucket| (bucket, empty_point(bucket)))
        .collect();
    for row in rows {
        if let Some(point) = points.get_mut(&row.bucket) {
            point.calls = row.calls;
            point.retries = row.retries;
            point.latency_ms = Latency {
                p50: row.p50_ms,
                p95: row.p95_ms,
                p99: row.p99_ms,
            };
            point.min_rate_limit_remaining = row.min_rate_limit_remaining;
        }
    }
    for row in outcomes {
        if let Some(point) = points.get_mut(&row.bucket) {
            point.by_outcome.insert(row.outcome, row.count);
        }
    }

    Ok(Json(Timeseries {
        provider,
        interval,
        from: first,
        to,
        points: points.into_values().collect(),
    }))
}

/// 업체별 요약 (대시보드 카드용)
#[utoipa::path(
    get,
    path = "/stats/providers/summary",
    operation_id = "admin_providers_summary",
    tag = tags::ADMIN_STATS,
    params(SummaryQuery),
    responses(
        (status = 200, description = "업체별 호출 수 · 성공률 · 지연", body = StatsSummary),
        (status = 400, description = "기간 값 오류", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.stats.summary", skip_all)]
pub async fn providers_summary(
    State(state): State<ApiState>,
    query: Result<Query<SummaryQuery>, QueryRejection>,
) -> Result<Json<StatsSummary>, ApiError> {
    let Query(query) = query?;
    let window = query.window.unwrap_or(SummaryWindow::OneHour);
    let to = Utc::now();
    let from = to - window.duration();

    let providers = db::providers::list_all(&state.pool).await?;
    let ids: Vec<String> = providers.iter().map(|p| p.id.to_string()).collect();
    let mut rows: BTreeMap<String, db::provider_calls::ProviderCallSummary> =
        db::provider_calls::summary(&state.pool, &ids, from)
            .await?
            .into_iter()
            .map(|row| (row.provider_id.clone(), row))
            .collect();

    let providers = providers
        .into_iter()
        .map(|provider| match rows.remove(provider.id.as_str()) {
            Some(row) => ProviderStats {
                provider: provider.id,
                attempts: row.attempts,
                called: row.called,
                answered: row.answered,
                rate_limited: row.rate_limited,
                blocked: row.blocked,
                retries: row.retries,
                success_rate: rate(row.answered, row.called),
                latency_ms: Latency {
                    p50: row.p50_ms,
                    p95: row.p95_ms,
                    p99: row.p99_ms,
                },
            },
            None => ProviderStats {
                provider: provider.id,
                attempts: 0,
                called: 0,
                answered: 0,
                rate_limited: 0,
                blocked: 0,
                retries: 0,
                success_rate: None,
                latency_ms: Latency {
                    p50: None,
                    p95: None,
                    p99: None,
                },
            },
        })
        .collect();
    Ok(Json(StatsSummary {
        window,
        from,
        to,
        providers,
    }))
}

async fn existing_provider(state: &ApiState, id: &str) -> Result<ProviderId, ApiError> {
    let not_found = || ApiError::ProviderNotFound(id.to_owned());
    let provider = ProviderId::parse(id).map_err(|_| not_found())?;
    db::providers::find(&state.pool, &provider)
        .await?
        .map(|record| record.id)
        .ok_or_else(not_found)
}

/// `time`이 속한 구간의 시작 시각. DB의 `date_bin(간격, 시각, '2000-01-01Z')`과 같은 경계다
/// (2000-01-01은 Unix 기준 시각(1970-01-01)에서 정확히 하루 단위만큼 떨어져 있어, 하루를 나누어떨어지게 하는 간격이면 경계가 같다).
fn align(time: DateTime<Utc>, step_secs: i64) -> DateTime<Utc> {
    // `div_euclid`: 음수에서도 아래로 내림하는 나눗셈 (1970년 이전 시각도 경계가 맞는다)
    let aligned = time.timestamp().div_euclid(step_secs) * step_secs;
    DateTime::from_timestamp(aligned, 0).unwrap_or(time)
}

fn empty_point(bucket: DateTime<Utc>) -> TimeseriesPoint {
    TimeseriesPoint {
        bucket,
        calls: 0,
        by_outcome: BTreeMap::new(),
        latency_ms: Latency {
            p50: None,
            p95: None,
            p99: None,
        },
        retries: 0,
        min_rate_limit_remaining: None,
    }
}

/// 비율 (분모가 0이면 `None`)
fn rate(part: i64, whole: i64) -> Option<f64> {
    // `as f64`: 정수를 실수로 바꾼다 (호출 수는 2^53보다 훨씬 작아 정밀도를 잃지 않는다)
    (whole > 0).then(|| part as f64 / whole as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 구간_경계는_간격의_배수로_내림한다() {
        let time: DateTime<Utc> = "2026-11-10T03:07:42Z".parse().unwrap();
        assert_eq!(align(time, 60).to_rfc3339(), "2026-11-10T03:07:00+00:00");
        assert_eq!(align(time, 300).to_rfc3339(), "2026-11-10T03:05:00+00:00");
        assert_eq!(align(time, 3_600).to_rfc3339(), "2026-11-10T03:00:00+00:00");
    }

    #[test]
    fn 비율은_분모가_0이면_없다() {
        assert_eq!(rate(1, 4), Some(0.25));
        assert_eq!(rate(0, 0), None);
    }

    #[test]
    fn 간격과_기간_이름은_쿼리_문자열_그대로다() {
        let interval: StatsInterval = serde_json::from_str("\"5m\"").unwrap();
        assert_eq!(interval, StatsInterval::FiveMinutes);
        let window: SummaryWindow = serde_json::from_str("\"24h\"").unwrap();
        assert_eq!(window.duration(), TimeDelta::hours(24));
    }
}
