//! 관리자 API (`/admin/api`, docs/04-api.md §3)
//!
//! 관리자 웹(web/admin)이 쓴다. 로그인한 관리자만 쓸 수 있고(세션 쿠키), 조작은 역할에 따라 막는다 (Phase 6):
//! 조회는 viewer 이상, 재처리 · 서킷 조작 · 장애 주입 · 대사는 operator 이상, 정책 변경은 admin (`crate::admin_auth`).
//! 조작은 모두 감사 로그(`admin_audit_log`)에 남고, 예약 상태를 바꾼 조작은 상태 이력에도 `admin:<이름>`으로 남는다.
//!
//! | 모듈 | 경로 |
//! |---|---|
//! | `auth` | 로그인 · 로그아웃 · 지금 세션 |
//! | `bookings` | 예약 목록 · 상세(상태 이력 · 호출 기록 · 작업), 실패한 예약 · 취소 실패 재시도 |
//! | `jobs` | 작업 목록 · 종류별 개수, dead 작업 재큐잉 |
//! | `providers` | 업체 목록 · 상세(정책 · 런타임 상태), 정책 변경, 서킷 강제 조작, 지금 대사 |
//! | `reconciliation` | 대사 이슈 목록 · 개수 · 처리 완료 표시 |
//! | `stats` | 업체 호출 통계: 구간별 시계열, 업체별 요약 (provider_calls) |
//! | `chaos` | mock 업체 장애 주입 중계 (개발 환경 전용) |
//! | `audit` | 감사 로그 |
//! | `info` | 관리자 화면이 알아야 할 서버 설정 (Grafana 주소, 쓸 수 있는 기능). 로그인 없이 읽는다 |
//! | `retry` | 재시도 · 재큐잉의 DB 처리 (한 트랜잭션) |

mod audit;
mod auth;
mod bookings;
mod chaos;
mod info;
mod jobs;
mod providers;
mod reconciliation;
mod retry;
mod stats;

use std::time::Instant;

use axum::http::Uri;
use chrono::{DateTime, TimeDelta, Utc};
use db::jobs::JobRecord;
use domain::BookingId;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{ApiError, ApiState};

pub use retry::{RetryOutcome, requeue_job, retry_booking};

/// 한 페이지의 기본 · 최대 크기
const DEFAULT_PAGE_SIZE: u32 = 50;
const MAX_PAGE_SIZE: u32 = 200;

pub(super) fn routes() -> OpenApiRouter<ApiState> {
    // 같은 경로에 메서드가 여러 개면(GET · PUT) 한 `routes!`에 함께 넣는다
    OpenApiRouter::new()
        .routes(routes!(auth::login))
        .routes(routes!(auth::logout))
        .routes(routes!(auth::session))
        .routes(routes!(bookings::list_bookings))
        .routes(routes!(bookings::get_booking))
        .routes(routes!(bookings::retry_booking))
        .routes(routes!(jobs::list_jobs))
        .routes(routes!(jobs::job_counts))
        .routes(routes!(jobs::requeue_job))
        .routes(routes!(providers::list_providers))
        .routes(routes!(providers::get_provider))
        .routes(routes!(providers::update_policy))
        .routes(routes!(providers::control_circuit))
        .routes(routes!(providers::run_reconcile))
        .routes(routes!(reconciliation::list_issues))
        .routes(routes!(reconciliation::issue_counts))
        .routes(routes!(reconciliation::resolve_issue))
        .routes(routes!(stats::providers_summary))
        .routes(routes!(stats::provider_timeseries))
        .routes(routes!(chaos::get_chaos, chaos::put_chaos))
        .routes(routes!(chaos::apply_chaos_preset))
        .routes(routes!(audit::audit_log))
        .routes(routes!(info::admin_info))
}

/// 관리자 조작을 감사 로그에 남긴다. 조작과 같은 트랜잭션(`conn`)에서 부르면 "조작은 됐는데 기록은 없는" 경우가 생기지 않는다.
/// 지금 요청의 trace ID를 함께 남긴다 (Grafana에서 그 조작의 처리 과정을 찾을 수 있다).
pub(super) async fn record_audit(
    conn: &mut PgConnection,
    actor: &str,
    action: &str,
    target: Option<&str>,
    detail: serde_json::Value,
) -> Result<(), ApiError> {
    let trace_id = telemetry::current_trace_id();
    db::admin::audit(
        conn,
        &db::admin::NewAuditEntry {
            actor,
            action,
            target,
            detail,
            trace_id: trace_id.as_deref(),
        },
    )
    .await?;
    Ok(())
}

/// 관리자 API에 없는 경로 (`/admin/api/...`)
pub(super) async fn not_found(uri: Uri) -> ApiError {
    ApiError::RouteNotFound(uri.path().to_owned())
}

// ───────────────────────── 여러 화면이 함께 쓰는 응답 모양 ─────────────────────────

/// 관리자 화면의 예약. 고객 정보(이름 · 전화번호)는 관리자 화면에도 내보내지 않는다.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AdminBooking {
    pub id: BookingId,
    /// 예약을 만든 API 클라이언트
    pub client_id: domain::ClientId,
    pub status: domain::BookingStatus,
    /// 마지막 상태 전이의 사유 (거절 코드, 포기 사유 등)
    pub status_reason: Option<String>,
    pub provider: domain::ProviderId,
    pub resource_id: String,
    pub slot_start: DateTime<Utc>,
    pub party_size: u32,
    /// 업체가 확정한 뒤 알려 준 업체 쪽 예약 ID
    pub provider_booking_id: Option<String>,
    /// 이 시각까지 확정하지 못하면 포기한다
    pub deadline_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<&domain::Booking> for AdminBooking {
    fn from(booking: &domain::Booking) -> Self {
        Self {
            id: booking.id,
            client_id: booking.client_id,
            status: booking.status,
            status_reason: booking.status_reason.clone(),
            provider: booking.request.provider.clone(),
            resource_id: booking.request.resource_id.clone(),
            slot_start: booking.request.slot_start,
            party_size: booking.request.party_size,
            provider_booking_id: booking.provider_booking_id.clone(),
            deadline_at: booking.deadline_at,
            created_at: booking.created_at,
            updated_at: booking.updated_at,
        }
    }
}

/// 작업 상태
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// 실행 대기 (`run_at`이 되면 워커가 가져간다)
    Queued,
    /// 워커가 처리 중
    Running,
    /// 끝남
    Done,
    /// 더 시도하지 않고 묻어 둠 (관리자 확인 대상)
    Dead,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Dead => "dead",
        }
    }

    fn parse(value: &str) -> Result<Self, ApiError> {
        [Self::Queued, Self::Running, Self::Done, Self::Dead]
            .into_iter()
            .find(|status| status.as_str() == value)
            .ok_or_else(|| ApiError::Internal(format!("알 수 없는 작업 상태: {value}")))
    }
}

/// 작업 한 건
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct JobView {
    pub id: i64,
    /// `submit_booking`, `resolve_unknown`, `cancel_booking`, `reconcile_provider` (이 서버 버전이 모르는 종류일 수도 있다)
    pub kind: String,
    pub booking_id: Option<BookingId>,
    /// 부를 업체 (대사 작업처럼 예약이 없는 작업도 업체는 있다)
    pub provider: Option<String>,
    pub status: JobStatus,
    /// 지금까지 시도한 횟수 (실행할 때마다 1 늘어난다. 업체를 부르지 않고 미룬 실행은 세지 않는다).
    /// lease 증표이기도 해서 되돌리지 않는다
    pub attempts: i32,
    /// `attempts`가 이 값에 이르면 포기한다. dead 작업을 되살리면 그 시점부터 기본 횟수(8)만큼 늘어난다
    pub max_attempts: i32,
    /// 실행하기로 한 시각
    pub run_at: DateTime<Utc>,
    /// 실행 중이면 이 시각까지 이 작업을 쥐고 있다 (lease)
    pub locked_until: Option<DateTime<Utc>>,
    /// 마지막 실패 · 미룸 사유
    pub last_error: Option<String>,
    /// 작업별 진행 상황 (예: `{"lookup_first": true}`, `{"not_found_checks": 1}`)
    #[schema(value_type = Object)]
    pub payload: serde_json::Value,
    /// 멱등키 없는 업체에 생성 요청을 보내고 아직 결과를 기록하지 못했으면 그 시각. 이 표시가 남은 작업을 다시 가져가면
    /// 워커는 다시 보내지 않고 결과 불명으로 넘긴다
    pub sent_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<JobRecord> for JobView {
    type Error = ApiError;

    fn try_from(job: JobRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            id: job.id,
            kind: job.kind,
            booking_id: job.booking_id,
            provider: job.provider_id,
            status: JobStatus::parse(&job.status)?,
            attempts: job.attempts,
            max_attempts: job.max_attempts,
            run_at: job.run_at,
            locked_until: job.locked_until,
            last_error: job.last_error,
            payload: job.payload,
            sent_at: job.sent_at,
            created_at: job.created_at,
            updated_at: job.updated_at,
        })
    }
}

/// 분위수 지연 (밀리초). 실제로 업체를 부른 시도가 없으면 `null`
#[derive(Debug, Clone, Copy, PartialEq, Serialize, ToSchema)]
pub struct Latency {
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
}

// ───────────────────────── 요청 해석 도우미 ─────────────────────────

/// DB의 업체 설정. 업체 ID 형식이 틀렸거나 없는 업체면 404.
async fn find_provider(
    state: &ApiState,
    id: &str,
) -> Result<db::providers::ProviderRecord, ApiError> {
    let not_found = || ApiError::ProviderNotFound(id.to_owned());
    let provider_id = domain::ProviderId::parse(id).map_err(|_| not_found())?;
    db::providers::find(&state.pool, &provider_id)
        .await?
        .ok_or_else(not_found)
}

/// 페이지 크기: 없으면 기본값, 너무 크면 최대값으로 맞춘다
fn page_size(limit: Option<u32>) -> Result<i64, ApiError> {
    match limit.unwrap_or(DEFAULT_PAGE_SIZE) {
        0 => Err(ApiError::InvalidRequest(
            "limit는 1 이상이어야 합니다".into(),
        )),
        n => Ok(i64::from(n.min(MAX_PAGE_SIZE))),
    }
}

/// 한 페이지를 자른다: `limit`보다 하나 더 읽어 와서 남는 것이 있으면 다음 페이지가 있다는 뜻이다.
/// 다음 페이지의 커서는 이 페이지의 마지막 항목에서 만든다.
fn paginate<T>(
    mut items: Vec<T>,
    limit: i64,
    cursor_of: impl Fn(&T) -> String,
) -> (Vec<T>, Option<String>) {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    if items.len() > limit {
        items.truncate(limit);
        let next = items.last().map(cursor_of);
        (items, next)
    } else {
        (items, None)
    }
}

/// 프로세스 안의 시각(`Instant`, 단조 시계)을 벽시계 시각(UTC)으로 바꾼다. 화면에 "언제까지"를 보여 줄 때 쓴다.
/// `Instant`는 "컴퓨터가 켜진 뒤 몇 초" 같은 값이라 그 자체로는 날짜가 아니다. 지금과의 차이를 지금 시각에 더한다.
/// API의 다른 시각처럼 마이크로초까지만 남긴다 (docs/04-api.md §1).
fn to_utc(at: Instant) -> DateTime<Utc> {
    let now = Instant::now();
    let utc_now = Utc::now();
    let delta = |d| TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX);
    let at = if at >= now {
        utc_now + delta(at - now)
    } else {
        utc_now - delta(now - at)
    };
    db::normalize_timestamp(at)
}
