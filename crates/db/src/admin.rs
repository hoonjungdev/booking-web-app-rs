//! 관리자 계정 · 로그인 세션 · 감사 로그 (`admin_users`, `admin_sessions`, `admin_audit_log`)
//!
//! 비밀번호 해시를 만들고 확인하는 일, 세션 토큰을 만드는 일은 booking-server가 한다. 여기서는 저장 · 조회만 한다.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{DbError, PgPool};

/// 관리자 계정
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminUser {
    pub id: Uuid,
    pub username: String,
    /// argon2 해시 (PHC 문자열)
    pub password_hash: String,
    /// `viewer`, `operator`, `admin` (DB의 CHECK 제약이 이 값만 허용한다)
    pub role: String,
    pub created_at: DateTime<Utc>,
}

/// 로그인 세션의 주인
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUser {
    pub user_id: Uuid,
    pub username: String,
    pub role: String,
    pub expires_at: DateTime<Utc>,
}

/// 감사 로그 한 줄
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry {
    pub id: i64,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub detail: serde_json::Value,
    pub trace_id: Option<String>,
    pub at: DateTime<Utc>,
}

/// 새로 남길 감사 로그
#[derive(Debug, Clone, PartialEq)]
pub struct NewAuditEntry<'a> {
    /// `admin:<이름>`
    pub actor: &'a str,
    /// `booking.retry`, `provider.policy.update` …
    pub action: &'a str,
    pub target: Option<&'a str>,
    pub detail: serde_json::Value,
    pub trace_id: Option<&'a str>,
}

/// 계정을 만든다. 같은 이름이 이미 있으면 DB의 유니크 제약 오류가 난다.
pub async fn create_user(
    pool: &PgPool,
    username: &str,
    password_hash: &str,
    role: &str,
) -> Result<AdminUser, DbError> {
    let user = sqlx::query_as!(
        AdminUser,
        "INSERT INTO admin_users (id, username, password_hash, role) VALUES ($1, $2, $3, $4)
         RETURNING id, username, password_hash, role, created_at",
        Uuid::now_v7(),
        username,
        password_hash,
        role,
    )
    .fetch_one(pool)
    .await?;
    Ok(user)
}

/// 이름으로 계정을 찾는다 (로그인)
#[tracing::instrument(name = "db.admin.find_user", skip_all)]
pub async fn find_user(pool: &PgPool, username: &str) -> Result<Option<AdminUser>, DbError> {
    let user = sqlx::query_as!(
        AdminUser,
        "SELECT id, username, password_hash, role, created_at FROM admin_users WHERE username = $1",
        username,
    )
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

/// 세션을 만든다. `token_hash`는 쿠키에 준 토큰의 SHA-256 해시다 (토큰 원문은 저장하지 않는다).
/// 그 사용자의 만료된 세션은 이때 함께 지운다 (쌓이지 않게).
#[tracing::instrument(name = "db.admin.create_session", skip_all)]
pub async fn create_session(
    pool: &PgPool,
    token_hash: &str,
    user_id: Uuid,
    expires_at: DateTime<Utc>,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "DELETE FROM admin_sessions WHERE user_id = $1 AND expires_at <= now()",
        user_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO admin_sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)",
        token_hash,
        user_id,
        expires_at,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 만료되지 않은 세션의 주인. 없거나 만료됐으면 `None`.
///
/// 관리자 API 요청마다 부른다. 기본 키(`token_hash`)로 한 건만 찾는다.
#[tracing::instrument(name = "db.admin.find_session", skip_all)]
pub async fn find_session(pool: &PgPool, token_hash: &str) -> Result<Option<SessionUser>, DbError> {
    let user = sqlx::query_as!(
        SessionUser,
        "SELECT s.user_id, u.username, u.role, s.expires_at
         FROM admin_sessions s JOIN admin_users u ON u.id = s.user_id
         WHERE s.token_hash = $1 AND s.expires_at > now()",
        token_hash,
    )
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

/// 세션을 지운다 (로그아웃)
#[tracing::instrument(name = "db.admin.delete_session", skip_all)]
pub async fn delete_session(pool: &PgPool, token_hash: &str) -> Result<(), DbError> {
    sqlx::query!(
        "DELETE FROM admin_sessions WHERE token_hash = $1",
        token_hash
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// 감사 로그를 남긴다. 조작과 같은 트랜잭션에서 부르면 "조작은 됐는데 기록이 없는" 경우가 생기지 않는다.
#[tracing::instrument(name = "db.admin.audit", skip_all, fields(action = entry.action))]
pub async fn audit(conn: &mut PgConnection, entry: &NewAuditEntry<'_>) -> Result<(), DbError> {
    sqlx::query!(
        "INSERT INTO admin_audit_log (actor, action, target, detail, trace_id)
         VALUES ($1, $2, $3, $4, $5)",
        entry.actor,
        entry.action,
        entry.target,
        &entry.detail,
        entry.trace_id,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// 감사 로그 (최근 것부터, 최대 `limit`건). `before`: 이 ID보다 앞선 것만 (커서 페이지네이션)
#[tracing::instrument(name = "db.admin.audit_log", skip_all)]
pub async fn audit_log(
    pool: &PgPool,
    before: Option<i64>,
    limit: i64,
) -> Result<Vec<AuditEntry>, DbError> {
    let entries = sqlx::query_as!(
        AuditEntry,
        "SELECT id, actor, action, target, detail, trace_id, at FROM admin_audit_log
         WHERE ($1::int8 IS NULL OR id < $1)
         ORDER BY id DESC
         LIMIT $2",
        before,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(entries)
}
