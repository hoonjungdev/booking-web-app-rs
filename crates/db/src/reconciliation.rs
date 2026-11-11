//! 대사(reconciliation)에서 찾은 불일치 (`reconciliation_issues`, docs/03-resilience.md §9)
//!
//! 대사 작업은 10분마다 같은 불일치를 다시 찾는다. 같은 미해결 이슈를 매번 새로 쌓지 않도록, 미해결 이슈는
//! (업체, 종류, 예약, 업체 예약)마다 하나만 있게 했다 (부분 유니크 인덱스 + `ON CONFLICT DO NOTHING`).

use std::str::FromStr;

use chrono::{DateTime, Utc};
use domain::{BookingId, ProviderId};
use sqlx::PgConnection;

use crate::{DbError, PgPool};

/// 불일치 종류
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IssueKind {
    /// 우리는 확정인데 업체에는 그 예약이 없다
    MissingAtProvider,
    /// 업체에 우리 참조값을 단 예약이 있는데 우리에게는 그 예약이 없다
    MissingLocally,
    /// 양쪽 상태가 다르다 (업체측 취소, 실패로 끝났는데 업체에서는 확정 등)
    StatusMismatch,
    /// 같은 참조값으로 확정된 예약이 업체에 2건 이상 있다 (이중 예약 의심, 긴급)
    DuplicateSuspected,
}

impl IssueKind {
    pub const ALL: [Self; 4] = [
        Self::MissingAtProvider,
        Self::MissingLocally,
        Self::StatusMismatch,
        Self::DuplicateSuspected,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingAtProvider => "missing_at_provider",
            Self::MissingLocally => "missing_locally",
            Self::StatusMismatch => "status_mismatch",
            Self::DuplicateSuspected => "duplicate_suspected",
        }
    }
}

impl FromStr for IssueKind {
    type Err = DbError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| DbError::corrupt(format!("알 수 없는 대사 이슈 종류: {value}")))
    }
}

/// 새로 기록할 이슈
#[derive(Debug, Clone, PartialEq)]
pub struct NewIssue {
    pub provider_id: ProviderId,
    pub booking_id: Option<BookingId>,
    pub provider_booking_id: Option<String>,
    pub kind: IssueKind,
    /// 무엇이 어떻게 달랐는지. 고객 정보는 넣지 않는다.
    pub detail: serde_json::Value,
    /// 대사가 자동으로 바로잡았으면 처리한 주체(`reconciler`). 그러면 처리 완료 상태로 기록한다.
    pub resolved_by: Option<String>,
}

/// 저장된 이슈
#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub id: i64,
    pub provider_id: String,
    pub booking_id: Option<BookingId>,
    pub provider_booking_id: Option<String>,
    pub kind: IssueKind,
    pub detail: serde_json::Value,
    pub detected_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolved_by: Option<String>,
}

/// 이슈 목록 조건. `None`인 조건은 걸지 않는다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueFilter {
    /// `Some(false)`: 미해결만, `Some(true)`: 처리 완료만
    pub resolved: Option<bool>,
    pub provider: Option<ProviderId>,
    pub kind: Option<IssueKind>,
    /// 이 ID보다 먼저 찾은 이슈만 (커서 페이지네이션)
    pub before: Option<i64>,
}

struct IssueRow {
    id: i64,
    provider_id: String,
    booking_id: Option<uuid::Uuid>,
    provider_booking_id: Option<String>,
    kind: String,
    detail: serde_json::Value,
    detected_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
    resolved_by: Option<String>,
}

impl TryFrom<IssueRow> for Issue {
    type Error = DbError;

    fn try_from(r: IssueRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: r.id,
            provider_id: r.provider_id,
            booking_id: r.booking_id.map(BookingId::from_uuid),
            provider_booking_id: r.provider_booking_id,
            kind: r.kind.parse()?,
            detail: r.detail,
            detected_at: r.detected_at,
            resolved_at: r.resolved_at,
            resolved_by: r.resolved_by,
        })
    }
}

/// 이슈를 기록한다. 같은 미해결 이슈가 이미 있으면 새로 넣지 않고 `None`.
///
/// 자동으로 바로잡은 이슈(`resolved_by`가 있음)는 처리 완료로 넣으므로 미해결 이슈와 겹치지 않고 항상 들어간다.
#[tracing::instrument(name = "db.reconciliation.record", skip_all, fields(kind = issue.kind.as_str()))]
pub async fn record(conn: &mut PgConnection, issue: &NewIssue) -> Result<Option<i64>, DbError> {
    let id = sqlx::query_scalar!(
        "INSERT INTO reconciliation_issues
             (provider_id, booking_id, provider_booking_id, kind, detail, resolved_at, resolved_by)
         VALUES ($1, $2, $3, $4, $5, CASE WHEN $6::text IS NULL THEN NULL ELSE now() END, $6)
         ON CONFLICT (provider_id, kind, booking_id, provider_booking_id) WHERE resolved_at IS NULL
         DO NOTHING
         RETURNING id",
        issue.provider_id.as_str(),
        issue.booking_id.map(|id| id.as_uuid()),
        issue.provider_booking_id.as_deref(),
        issue.kind.as_str(),
        &issue.detail,
        issue.resolved_by.as_deref(),
    )
    .fetch_optional(conn)
    .await?;
    Ok(id)
}

/// 이슈 목록 (최근에 찾은 것부터, 최대 `limit`건)
#[tracing::instrument(name = "db.reconciliation.list", skip_all)]
pub async fn list(pool: &PgPool, filter: &IssueFilter, limit: i64) -> Result<Vec<Issue>, DbError> {
    let rows = sqlx::query_as!(
        IssueRow,
        r#"
        SELECT id, provider_id, booking_id, provider_booking_id, kind, detail, detected_at,
               resolved_at, resolved_by
        FROM reconciliation_issues
        WHERE ($1::bool IS NULL OR (resolved_at IS NOT NULL) = $1)
          AND ($2::text IS NULL OR provider_id = $2)
          AND ($3::text IS NULL OR kind = $3)
          AND ($4::int8 IS NULL OR id < $4)
        ORDER BY id DESC
        LIMIT $5
        "#,
        filter.resolved,
        filter.provider.as_ref().map(ProviderId::as_str),
        filter.kind.map(IssueKind::as_str),
        filter.before,
        limit,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(Issue::try_from).collect()
}

/// 이슈 하나
#[tracing::instrument(name = "db.reconciliation.get", skip_all)]
pub async fn get(conn: &mut PgConnection, id: i64) -> Result<Option<Issue>, DbError> {
    let row = sqlx::query_as!(
        IssueRow,
        "SELECT id, provider_id, booking_id, provider_booking_id, kind, detail, detected_at,
                resolved_at, resolved_by
         FROM reconciliation_issues WHERE id = $1",
        id,
    )
    .fetch_optional(conn)
    .await?;
    row.map(Issue::try_from).transpose()
}

/// 처리 완료로 표시한다. 이미 처리 완료였으면 아무것도 하지 않고 `false`.
#[tracing::instrument(name = "db.reconciliation.resolve", skip_all)]
pub async fn resolve(conn: &mut PgConnection, id: i64, by: &str) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE reconciliation_issues SET resolved_at = now(), resolved_by = $2
         WHERE id = $1 AND resolved_at IS NULL",
        id,
        by,
    )
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 미해결 이슈 수 (종류별). 이슈가 없는 종류는 결과에 없다.
pub async fn open_counts(pool: &PgPool) -> Result<Vec<(IssueKind, i64)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT kind, count(*) AS "count!" FROM reconciliation_issues
           WHERE resolved_at IS NULL GROUP BY kind ORDER BY kind"#,
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| Ok((r.kind.parse()?, r.count)))
        .collect()
}
