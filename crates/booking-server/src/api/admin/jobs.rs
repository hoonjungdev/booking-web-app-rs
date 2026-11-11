//! 관리자: 작업 큐 (목록 · 종류별 개수 · dead 작업 재큐잉)

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{PathRejection, QueryRejection},
    },
    http::StatusCode,
};
use db::jobs::{JobFilter, JobKind, KindCounts};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{JobStatus, JobView, bookings::RetryResult, page_size, paginate};
use crate::{
    admin_auth::{AdminSession, RequireOperator},
    api::{ApiError, ApiState, Problem, docs::tags},
    trace,
};

/// 작업 목록 조건. 모두 생략할 수 있다.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct JobListQuery {
    /// 작업 상태
    status: Option<JobStatus>,
    /// 작업 종류 (`submit_booking`, `resolve_unknown`, `cancel_booking`, `reconcile_provider`)
    kind: Option<String>,
    /// 다음 페이지: 앞 페이지 응답의 `next_cursor`
    cursor: Option<String>,
    /// 한 페이지 크기 (기본 50, 최대 200)
    limit: Option<u32>,
}

/// 작업 목록 한 페이지
#[derive(Debug, Serialize, ToSchema)]
pub struct JobPage {
    /// 최근에 등록한 것부터
    pub items: Vec<JobView>,
    /// 다음 페이지를 읽을 커서. 마지막 페이지면 `null`
    pub next_cursor: Option<String>,
}

/// 상태별 작업 수 (끝난 작업은 세지 않는다)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct QueueCounts {
    /// 지금 실행할 수 있는데 아직 가져가지 않은 작업
    pub ready: i64,
    /// 나중에 실행하기로 미뤄 둔 작업 (재시도 대기 등)
    pub scheduled: i64,
    /// 워커가 처리 중인 작업
    pub running: i64,
    /// 더 시도하지 않고 묻어 둔 작업 (관리자 확인 대상)
    pub dead: i64,
}

/// 종류 하나의 작업 수
#[derive(Debug, Serialize, ToSchema)]
pub struct KindQueueCounts {
    pub kind: String,
    pub counts: QueueCounts,
}

/// 작업 큐 개수: 종류별과 전체
#[derive(Debug, Serialize, ToSchema)]
pub struct JobCounts {
    /// 이 서버가 아는 종류는 작업이 없어도 0으로 나온다
    pub kinds: Vec<KindQueueCounts>,
    pub total: QueueCounts,
}

/// 작업 목록
#[utoipa::path(
    get,
    path = "/jobs",
    operation_id = "admin_list_jobs",
    tag = tags::ADMIN_JOBS,
    security(("admin_session" = [])),
    params(JobListQuery),
    responses(
        (status = 200, description = "작업 목록 한 페이지", body = JobPage),
        (status = 401, description = "로그인 필요", body = Problem, content_type = "application/problem+json"),
        (status = 400, description = "조건 값 오류", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.jobs.list", skip_all)]
pub async fn list_jobs(
    State(state): State<ApiState>,
    _session: AdminSession,
    query: Result<Query<JobListQuery>, QueryRejection>,
) -> Result<Json<JobPage>, ApiError> {
    let Query(query) = query?;
    let limit = page_size(query.limit)?;
    let before = query
        .cursor
        .as_deref()
        .map(|cursor| {
            cursor
                .parse::<i64>()
                .map_err(|_| ApiError::InvalidRequest("cursor 값이 올바르지 않습니다".into()))
        })
        .transpose()?;
    let filter = JobFilter {
        status: query.status.map(|status| status.as_str().to_owned()),
        kind: query.kind,
        before,
    };
    let jobs = db::jobs::list(&state.pool, &filter, limit + 1).await?;
    let items = jobs
        .into_iter()
        .map(JobView::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    let (items, next_cursor) = paginate(items, limit, |job| job.id.to_string());
    Ok(Json(JobPage { items, next_cursor }))
}

/// 작업 종류 · 상태별 개수
#[utoipa::path(
    get,
    path = "/jobs/counts",
    operation_id = "admin_job_counts",
    tag = tags::ADMIN_JOBS,
    security(("admin_session" = [])),
    responses(
        (status = 200, description = "종류별 · 전체 작업 수", body = JobCounts),
        (status = 401, description = "로그인 필요", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.jobs.counts", skip_all)]
pub async fn job_counts(
    State(state): State<ApiState>,
    _session: AdminSession,
) -> Result<Json<JobCounts>, ApiError> {
    let counts = db::jobs::counts(&state.pool).await?;
    Ok(Json(summarize(counts)))
}

/// dead 작업을 다시 대기열에 넣는다
///
/// 작업의 예약이 `failed` · `cancel_failed`면 예약 재시도(`POST /bookings/{id}/retry`)와 같다: 예약을 되돌리고
/// 이 작업을 "업체에서 먼저 찾아본 뒤 제출하는" `submit_booking`(또는 다시 취소하는 `cancel_booking`)으로 되살린다.
/// 그 밖에는 예약은 그대로 두고 작업만 같은 내용으로 되살린다. 시도 횟수는 그대로 두고 한도를 늘린다.
/// 포기한 대사 작업(`reconcile_provider`)은 업체마다 하나뿐인 대사 자리가 이미 차 있어 되살리지 않는다:
/// 그 행은 끝난 것으로 정리하고, 그 업체의 대사 작업을 지금으로 당겨 그 작업을 돌려준다.
#[utoipa::path(
    post,
    path = "/jobs/{id}/requeue",
    operation_id = "admin_requeue_job",
    tag = tags::ADMIN_JOBS,
    security(("admin_session" = [])),
    params(("id" = i64, Path, description = "작업 ID")),
    responses(
        (status = 202, description = "대기열에 다시 넣음", body = RetryResult),
        (status = 401, description = "로그인 필요", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "operator 권한 필요", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "없는 작업", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "dead가 아니거나, 같은 예약에 처리 중인 작업이 있음", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.jobs.requeue", skip_all, fields(job_id))]
pub async fn requeue_job(
    State(state): State<ApiState>,
    RequireOperator(session): RequireOperator,
    id: Result<Path<i64>, PathRejection>,
) -> Result<(StatusCode, Json<RetryResult>), ApiError> {
    let Path(id) = id?;
    tracing::Span::current().record("job_id", id);
    let outcome = super::requeue_job(&state.pool, id, &trace::current(), &session.actor()).await?;
    Ok((StatusCode::ACCEPTED, Json(outcome.try_into()?)))
}

/// DB의 종류별 개수를 화면용으로 정리한다. 이 서버가 아는 종류는 작업이 없어도 0으로 채운다.
fn summarize(counts: Vec<KindCounts>) -> JobCounts {
    // BTreeMap: 키 순서대로 정렬되는 맵 (≈ C#의 SortedDictionary)
    let mut kinds: BTreeMap<String, QueueCounts> = JobKind::ALL
        .into_iter()
        .map(|kind| (kind.as_str().to_owned(), QueueCounts::default()))
        .collect();
    for row in counts {
        kinds.insert(
            row.kind,
            QueueCounts {
                ready: row.ready,
                scheduled: row.scheduled,
                running: row.running,
                dead: row.dead,
            },
        );
    }
    let total = kinds
        .values()
        .fold(QueueCounts::default(), |sum, c| QueueCounts {
            ready: sum.ready + c.ready,
            scheduled: sum.scheduled + c.scheduled,
            running: sum.running + c.running,
            dead: sum.dead + c.dead,
        });
    JobCounts {
        kinds: kinds
            .into_iter()
            .map(|(kind, counts)| KindQueueCounts { kind, counts })
            .collect(),
        total,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn 아는_종류는_0으로_채우고_모르는_종류도_보여_준다() {
        let counts = summarize(vec![
            KindCounts {
                kind: "submit_booking".into(),
                ready: 2,
                scheduled: 1,
                running: 0,
                dead: 3,
            },
            KindCounts {
                kind: "something_new".into(),
                ready: 0,
                scheduled: 0,
                running: 0,
                dead: 1,
            },
        ]);
        let kinds: Vec<_> = counts.kinds.iter().map(|k| k.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "cancel_booking",
                "reconcile_provider",
                "resolve_unknown",
                "something_new",
                "submit_booking"
            ]
        );
        assert_eq!(counts.total.dead, 4);
        assert_eq!(counts.total.ready, 2);
        assert!(JobKind::from_str("resolve_unknown").is_ok());
    }
}
