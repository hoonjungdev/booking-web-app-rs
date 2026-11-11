//! 관리자: 감사 로그 (관리자 조작 기록)

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{page_size, paginate};
use crate::{
    admin_auth::AdminSession,
    api::{ApiError, ApiState, Problem, docs::tags},
};

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AuditQuery {
    /// 다음 페이지: 앞 페이지 응답의 `next_cursor`
    cursor: Option<String>,
    /// 한 페이지 크기 (기본 50, 최대 200)
    limit: Option<u32>,
}

/// 감사 로그 한 줄
#[derive(Debug, Serialize, ToSchema)]
pub struct AuditEntryView {
    pub id: i64,
    /// `admin:<이름>`
    pub actor: String,
    /// `booking.retry`, `provider.policy.update` …
    pub action: String,
    /// 조작 대상 (예약 ID, 업체 ID, 작업 ID 등)
    pub target: Option<String>,
    #[schema(value_type = Object)]
    pub detail: serde_json::Value,
    /// 조작 요청의 trace
    pub trace_id: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AuditPage {
    /// 최근 것부터
    pub items: Vec<AuditEntryView>,
    pub next_cursor: Option<String>,
}

/// 감사 로그 (관리자 조작 기록)
#[utoipa::path(
    get,
    path = "/audit",
    operation_id = "admin_audit_log",
    tag = tags::ADMIN_AUTH,
    security(("admin_session" = [])),
    params(AuditQuery),
    responses(
        (status = 200, description = "감사 로그 한 페이지", body = AuditPage),
        (status = 400, description = "조건 값 오류", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "로그인 필요", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "admin.audit.list", skip_all)]
pub async fn audit_log(
    State(state): State<ApiState>,
    _session: AdminSession,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Result<Json<AuditPage>, ApiError> {
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
    let entries = db::admin::audit_log(&state.pool, before, limit + 1).await?;
    let items: Vec<AuditEntryView> = entries
        .into_iter()
        .map(|entry| AuditEntryView {
            id: entry.id,
            actor: entry.actor,
            action: entry.action,
            target: entry.target,
            detail: entry.detail,
            trace_id: entry.trace_id,
            at: entry.at,
        })
        .collect();
    let (items, next_cursor) = paginate(items, limit, |entry| entry.id.to_string());
    Ok(Json(AuditPage { items, next_cursor }))
}
