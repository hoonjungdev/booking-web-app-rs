//! 관리자: 대사 이슈 목록 · 개수 · 처리 완료 표시 (docs/03-resilience.md §9)

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{PathRejection, QueryRejection},
    },
};
use chrono::{DateTime, Utc};
use db::reconciliation::{Issue, IssueFilter, IssueKind};
use domain::{BookingId, ProviderId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{page_size, paginate, record_audit};
use crate::{
    admin_auth::{AdminSession, RequireOperator},
    api::{ApiError, ApiState, Problem, docs::tags},
};

/// 대사 이슈 종류
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueKindView {
    /// 우리는 확정인데 업체에는 그 예약이 없다
    MissingAtProvider,
    /// 업체에 우리 참조값을 단 예약이 있는데 우리에게는 없다
    MissingLocally,
    /// 양쪽 상태가 다르다 (업체측 취소 등. 대사가 자동으로 바로잡은 것은 처리 완료로 기록된다)
    StatusMismatch,
    /// 같은 참조값으로 확정된 예약이 업체에 2건 이상 (이중 예약 의심, 긴급)
    DuplicateSuspected,
}

impl From<IssueKind> for IssueKindView {
    fn from(kind: IssueKind) -> Self {
        match kind {
            IssueKind::MissingAtProvider => Self::MissingAtProvider,
            IssueKind::MissingLocally => Self::MissingLocally,
            IssueKind::StatusMismatch => Self::StatusMismatch,
            IssueKind::DuplicateSuspected => Self::DuplicateSuspected,
        }
    }
}

impl From<IssueKindView> for IssueKind {
    fn from(kind: IssueKindView) -> Self {
        match kind {
            IssueKindView::MissingAtProvider => Self::MissingAtProvider,
            IssueKindView::MissingLocally => Self::MissingLocally,
            IssueKindView::StatusMismatch => Self::StatusMismatch,
            IssueKindView::DuplicateSuspected => Self::DuplicateSuspected,
        }
    }
}

/// 이슈 목록 조건. 모두 생략할 수 있다.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct IssueListQuery {
    /// `false`: 미해결만 (기본), `true`: 처리 완료만
    resolved: Option<bool>,
    /// 업체 ID
    provider: Option<String>,
    /// 이슈 종류
    kind: Option<IssueKindView>,
    /// 다음 페이지: 앞 페이지 응답의 `next_cursor`
    cursor: Option<String>,
    /// 한 페이지 크기 (기본 50, 최대 200)
    limit: Option<u32>,
}

/// 대사 이슈 하나
#[derive(Debug, Serialize, ToSchema)]
pub struct IssueView {
    pub id: i64,
    pub provider: String,
    /// 우리 예약 (우리 쪽에 없는 예약이면 업체가 알려 준 참조값)
    pub booking_id: Option<BookingId>,
    pub provider_booking_id: Option<String>,
    pub kind: IssueKindView,
    /// 무엇이 어떻게 달랐는지 (우리 상태, 업체 상태, 업체 예약 ID 목록 등)
    #[schema(value_type = Object)]
    pub detail: serde_json::Value,
    pub detected_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    /// `reconciler`(대사가 자동으로 바로잡음) 또는 `admin:<이름>`
    pub resolved_by: Option<String>,
}

impl From<Issue> for IssueView {
    fn from(issue: Issue) -> Self {
        Self {
            id: issue.id,
            provider: issue.provider_id,
            booking_id: issue.booking_id,
            provider_booking_id: issue.provider_booking_id,
            kind: issue.kind.into(),
            detail: issue.detail,
            detected_at: issue.detected_at,
            resolved_at: issue.resolved_at,
            resolved_by: issue.resolved_by,
        }
    }
}

/// 이슈 목록 한 페이지
#[derive(Debug, Serialize, ToSchema)]
pub struct IssuePage {
    /// 최근에 찾은 것부터
    pub items: Vec<IssueView>,
    pub next_cursor: Option<String>,
}

/// 미해결 이슈 수
#[derive(Debug, Serialize, ToSchema)]
pub struct IssueCounts {
    pub open: i64,
    /// 종류별 (이슈가 없는 종류도 0으로 들어 있다)
    pub by_kind: BTreeMap<String, i64>,
}

/// 대사 이슈 목록
#[utoipa::path(
    get,
    path = "/reconciliation/issues",
    operation_id = "admin_list_issues",
    tag = tags::ADMIN_RECONCILIATION,
    security(("admin_session" = [])),
    params(IssueListQuery),
    responses(
        (status = 200, description = "이슈 목록 한 페이지", body = IssuePage),
        (status = 400, description = "조건 값 오류", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "로그인 필요", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.reconciliation.list", skip_all)]
pub async fn list_issues(
    State(state): State<ApiState>,
    _session: AdminSession,
    query: Result<Query<IssueListQuery>, QueryRejection>,
) -> Result<Json<IssuePage>, ApiError> {
    let Query(query) = query?;
    let limit = page_size(query.limit)?;
    let filter = IssueFilter {
        resolved: Some(query.resolved.unwrap_or(false)),
        provider: query
            .provider
            .as_deref()
            .map(ProviderId::parse)
            .transpose()
            .map_err(|error| ApiError::InvalidRequest(error.to_string()))?,
        kind: query.kind.map(IssueKind::from),
        before: query
            .cursor
            .as_deref()
            .map(|cursor| {
                cursor
                    .parse::<i64>()
                    .map_err(|_| ApiError::InvalidRequest("cursor 값이 올바르지 않습니다".into()))
            })
            .transpose()?,
    };
    let issues = db::reconciliation::list(&state.pool, &filter, limit + 1).await?;
    let items: Vec<IssueView> = issues.into_iter().map(IssueView::from).collect();
    let (items, next_cursor) = paginate(items, limit, |issue| issue.id.to_string());
    Ok(Json(IssuePage { items, next_cursor }))
}

/// 미해결 대사 이슈 수 (대시보드)
#[utoipa::path(
    get,
    path = "/reconciliation/counts",
    operation_id = "admin_issue_counts",
    tag = tags::ADMIN_RECONCILIATION,
    security(("admin_session" = [])),
    responses(
        (status = 200, description = "미해결 이슈 수", body = IssueCounts),
        (status = 401, description = "로그인 필요", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.reconciliation.counts", skip_all)]
pub async fn issue_counts(
    State(state): State<ApiState>,
    _session: AdminSession,
) -> Result<Json<IssueCounts>, ApiError> {
    let mut by_kind: BTreeMap<String, i64> = IssueKind::ALL
        .into_iter()
        .map(|kind| (kind.as_str().to_owned(), 0))
        .collect();
    for (kind, count) in db::reconciliation::open_counts(&state.pool).await? {
        by_kind.insert(kind.as_str().to_owned(), count);
    }
    Ok(Json(IssueCounts {
        open: by_kind.values().sum(),
        by_kind,
    }))
}

/// 이슈를 처리 완료로 표시한다 (관리자가 업체와 확인해 정리한 뒤)
#[utoipa::path(
    post,
    path = "/reconciliation/issues/{id}/resolve",
    operation_id = "admin_resolve_issue",
    tag = tags::ADMIN_RECONCILIATION,
    security(("admin_session" = [])),
    params(("id" = i64, Path, description = "이슈 ID")),
    responses(
        (status = 200, description = "처리 완료로 표시한 이슈", body = IssueView),
        (status = 401, description = "로그인 필요", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "operator 권한 필요", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "없는 이슈", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "이미 처리 완료", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.reconciliation.resolve", skip_all, fields(issue_id))]
pub async fn resolve_issue(
    State(state): State<ApiState>,
    RequireOperator(session): RequireOperator,
    id: Result<Path<i64>, PathRejection>,
) -> Result<Json<IssueView>, ApiError> {
    let Path(id) = id?;
    tracing::Span::current().record("issue_id", id);
    let actor = session.actor();
    let mut tx = state.pool.begin().await?;
    let issue = db::reconciliation::get(&mut tx, id)
        .await?
        .ok_or(ApiError::IssueNotFound)?;
    if !db::reconciliation::resolve(&mut tx, id, &actor).await? {
        return Err(ApiError::Conflict(format!(
            "이미 처리 완료로 표시된 이슈입니다 ({})",
            issue.resolved_by.as_deref().unwrap_or("?")
        )));
    }
    let detail = serde_json::json!({ "kind": issue.kind.as_str(), "provider": issue.provider_id });
    record_audit(
        &mut tx,
        &actor,
        "reconciliation.resolve",
        Some(&id.to_string()),
        detail,
    )
    .await?;
    let resolved = db::reconciliation::get(&mut tx, id)
        .await?
        .ok_or(ApiError::IssueNotFound)?;
    tx.commit().await?;
    tracing::info!(issue_id = id, %actor, "관리자가 대사 이슈를 처리 완료로 표시했다");
    Ok(Json(resolved.into()))
}
