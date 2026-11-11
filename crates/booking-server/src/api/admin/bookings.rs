//! 관리자: 예약 목록 · 상세 · 재시도

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{PathRejection, QueryRejection},
    },
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use db::bookings::BookingFilter;
use domain::{BookingId, BookingStatus, ProviderId};
use serde::{Deserialize, Serialize};
use tracing::{Span, field::Empty};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{AdminBooking, JobView, page_size, paginate};
use crate::{
    api::{ApiError, ApiState, Problem, docs::tags},
    trace,
};

/// 예약 목록 조건. 모두 생략할 수 있다.
///
/// `#[derive(utoipa::IntoParams)]`: 이 구조체의 필드를 API 문서의 쿼리 매개변수 목록으로 만든다.
/// (`use utoipa::IntoParams`로 가져오지 않고 전체 경로로 쓴다. 가져오면 매크로가 만든 코드의 `utoipa::IntoParams`가
/// "불필요한 경로 표기" 린트(`unused_qualifications`)에 걸린다.)
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BookingListQuery {
    /// 예약 상태
    status: Option<BookingStatus>,
    /// 업체 ID
    provider: Option<String>,
    /// 접수 시각이 이 시각 이후 (포함, RFC 3339)
    from: Option<DateTime<Utc>>,
    /// 접수 시각이 이 시각 전 (미포함, RFC 3339)
    to: Option<DateTime<Utc>>,
    /// 다음 페이지: 앞 페이지 응답의 `next_cursor`를 그대로 넣는다
    cursor: Option<String>,
    /// 한 페이지 크기 (기본 50, 최대 200)
    limit: Option<u32>,
}

/// 예약 목록 한 페이지
#[derive(Debug, Serialize, ToSchema)]
pub struct BookingPage {
    /// 최근에 접수한 것부터
    pub items: Vec<AdminBooking>,
    /// 다음 페이지를 읽을 커서. 마지막 페이지면 `null`. 내용을 해석하지 말고 그대로 돌려보낸다.
    pub next_cursor: Option<String>,
}

/// 예약 상세: 예약, 상태 이력, 업체 호출 기록, 작업
#[derive(Debug, Serialize, ToSchema)]
pub struct BookingDetail {
    pub booking: AdminBooking,
    /// 상태 변경 이력 (오래된 것부터)
    pub events: Vec<BookingEventView>,
    /// 업체 호출 기록 (시도 1번 = 1행, 오래된 것부터)
    pub calls: Vec<ProviderCallView>,
    /// 이 예약을 처리한 작업 (등록 순서대로)
    pub jobs: Vec<JobView>,
}

/// 상태 이력 한 줄
#[derive(Debug, Serialize, ToSchema)]
pub struct BookingEventView {
    /// 바뀌기 전 상태 (접수는 `null`)
    pub from_status: Option<BookingStatus>,
    pub to_status: BookingStatus,
    pub reason: Option<String>,
    /// 바꾼 주체: `api`, `worker`, `admin` …
    pub actor: String,
    /// 이 변경이 일어난 trace (접수는 요청의 trace, 확정 · 실패는 작업의 trace)
    pub trace_id: Option<String>,
    pub at: DateTime<Utc>,
}

/// 업체 호출 시도 한 번
#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderCallView {
    pub id: i64,
    /// `create_booking`, `find_booking` …
    pub operation: String,
    pub job_id: Option<i64>,
    /// 호출 안에서 몇 번째 시도인가 (1부터)
    pub attempt: i32,
    pub started_at: DateTime<Utc>,
    pub duration_ms: i32,
    /// `success` 또는 실패 분류 (docs/03-resilience.md §1)
    pub outcome: String,
    pub http_status: Option<i32>,
    /// 업체가 알려 준 남은 호출 수
    pub rate_limit_remaining: Option<i32>,
    pub retry_after_ms: Option<i32>,
    /// 원인을 찾는 데 필요한 자세한 내용 (업체가 보낸 오류 본문 등)
    pub error_detail: Option<String>,
    /// 이 시도가 속한 trace
    pub trace_id: Option<String>,
}

/// 재시도 · 재큐잉 결과
#[derive(Debug, Serialize, ToSchema)]
pub struct RetryResult {
    /// 다시 처리하게 된 예약. 예약과 상관없는 작업을 되살렸으면 `null`
    pub booking: Option<AdminBooking>,
    /// 대기열에 들어간 작업
    pub job: JobView,
}

impl TryFrom<super::RetryOutcome> for RetryResult {
    type Error = ApiError;

    fn try_from(outcome: super::RetryOutcome) -> Result<Self, Self::Error> {
        Ok(Self {
            booking: outcome.booking.as_ref().map(AdminBooking::from),
            job: outcome.job.try_into()?,
        })
    }
}

/// 예약 목록
#[utoipa::path(
    get,
    path = "/bookings",
    operation_id = "admin_list_bookings",
    tag = tags::ADMIN_BOOKINGS,
    params(BookingListQuery),
    responses(
        (status = 200, description = "예약 목록 한 페이지", body = BookingPage),
        (status = 400, description = "조건 값 오류", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.bookings.list", skip_all)]
pub async fn list_bookings(
    State(state): State<ApiState>,
    query: Result<Query<BookingListQuery>, QueryRejection>,
) -> Result<Json<BookingPage>, ApiError> {
    let Query(query) = query?;
    let limit = page_size(query.limit)?;
    let filter = BookingFilter {
        status: query.status,
        provider: query
            .provider
            .as_deref()
            .map(ProviderId::parse)
            .transpose()
            .map_err(|error| ApiError::InvalidRequest(error.to_string()))?,
        created_from: query.from,
        created_to: query.to,
        before: query.cursor.as_deref().map(parse_cursor).transpose()?,
    };
    // 한 건 더 읽어 다음 페이지가 있는지 본다
    let bookings = db::bookings::list(&state.pool, &filter, limit + 1).await?;
    let items: Vec<AdminBooking> = bookings.iter().map(AdminBooking::from).collect();
    let (items, next_cursor) = paginate(items, limit, |booking| booking.id.to_string());
    Ok(Json(BookingPage { items, next_cursor }))
}

/// 예약 상세
#[utoipa::path(
    get,
    path = "/bookings/{id}",
    operation_id = "admin_get_booking",
    tag = tags::ADMIN_BOOKINGS,
    params(("id" = Uuid, Path, description = "예약 ID")),
    responses(
        (status = 200, description = "예약과 상태 이력 · 업체 호출 기록 · 작업", body = BookingDetail),
        (status = 404, description = "없는 예약", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.bookings.get", skip_all, fields(booking_id = Empty))]
pub async fn get_booking(
    State(state): State<ApiState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<BookingDetail>, ApiError> {
    let id = booking_id(id)?;
    let booking = db::bookings::get(&state.pool, id)
        .await?
        .ok_or(ApiError::BookingNotFound)?;
    let events = db::bookings::events(&state.pool, id).await?;
    let calls = db::provider_calls::for_booking(&state.pool, id).await?;
    let jobs = db::jobs::for_booking(&state.pool, id).await?;

    Ok(Json(BookingDetail {
        booking: AdminBooking::from(&booking),
        events: events
            .into_iter()
            .map(|event| BookingEventView {
                from_status: event.from_status,
                to_status: event.to_status,
                reason: event.reason,
                actor: event.actor,
                trace_id: event.trace_id,
                at: event.at,
            })
            .collect(),
        calls: calls
            .into_iter()
            .map(|call| ProviderCallView {
                id: call.id,
                operation: call.operation,
                job_id: call.job_id,
                attempt: call.attempt,
                started_at: call.started_at,
                duration_ms: call.duration_ms,
                outcome: call.outcome,
                http_status: call.http_status,
                rate_limit_remaining: call.rate_limit_remaining,
                retry_after_ms: call.retry_after_ms,
                error_detail: call.error_detail,
                trace_id: call.trace_id,
            })
            .collect(),
        // `collect::<Result<Vec<_>, _>>()`: 하나라도 바꾸지 못하면 그 에러를 돌려준다
        jobs: jobs
            .into_iter()
            .map(JobView::try_from)
            .collect::<Result<_, _>>()?,
    }))
}

/// 실패한 예약을 다시 처리한다
///
/// 예약을 `failed` → `pending`으로 되돌리고, 그 예약의 dead 작업을 되살린다 (없으면 새로 등록).
/// 워커는 다시 보내기 전에 업체에서 이 예약을 먼저 찾아본다 (이미 만들어졌으면 확정으로 끝낸다).
#[utoipa::path(
    post,
    path = "/bookings/{id}/retry",
    operation_id = "admin_retry_booking",
    tag = tags::ADMIN_BOOKINGS,
    params(("id" = Uuid, Path, description = "예약 ID")),
    responses(
        (status = 202, description = "다시 처리하도록 작업을 넣음", body = RetryResult),
        (status = 404, description = "없는 예약", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "failed가 아니거나 처리 중인 작업이 있음", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.bookings.retry", skip_all, fields(booking_id = Empty))]
pub async fn retry_booking(
    State(state): State<ApiState>,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<(StatusCode, Json<RetryResult>), ApiError> {
    let id = booking_id(id)?;
    let outcome = super::retry_booking(&state.pool, id, &trace::current()).await?;
    Ok((StatusCode::ACCEPTED, Json(outcome.try_into()?)))
}

/// 경로의 예약 ID. 형식이 틀리면 "없는 예약"으로 본다. 지금 span에 예약 ID를 남긴다.
fn booking_id(id: Result<Path<Uuid>, PathRejection>) -> Result<BookingId, ApiError> {
    let Path(id) = id.map_err(|_| ApiError::BookingNotFound)?;
    Span::current().record("booking_id", tracing::field::display(id));
    Ok(BookingId::from_uuid(id))
}

/// 커서 → 예약 ID. 커서는 이 페이지의 마지막 예약 ID다 (클라이언트는 내용을 해석하지 않는다).
fn parse_cursor(cursor: &str) -> Result<BookingId, ApiError> {
    Uuid::parse_str(cursor)
        .map(BookingId::from_uuid)
        .map_err(|_| ApiError::InvalidRequest("cursor 값이 올바르지 않습니다".into()))
}
