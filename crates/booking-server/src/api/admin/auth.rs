//! 관리자: 로그인 · 로그아웃 · 지금 세션 (docs/06-admin-frontend.md §6)

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::record_audit;
use crate::{
    admin_auth::{self, AdminRole, AdminSession},
    api::{ApiError, ApiState, Problem, docs::tags},
};

/// 로그인 요청. 비밀번호가 로그에 찍히지 않도록 `Debug`를 만들지 않는다.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// 로그인한 관리자
#[derive(Debug, Serialize, ToSchema)]
pub struct SessionView {
    pub username: String,
    pub role: AdminRole,
    /// 세션이 끝나는 시각. 지나면 다시 로그인해야 한다
    pub expires_at: DateTime<Utc>,
}

/// 로그인한다. 성공하면 세션 쿠키(`HttpOnly`, `SameSite=Strict`)를 준다.
///
/// 아이디가 없는지 비밀번호가 틀렸는지는 구분해 알려 주지 않는다 (계정 이름 추측을 막는다).
#[utoipa::path(
    post,
    path = "/login",
    operation_id = "admin_login",
    tag = tags::ADMIN_AUTH,
    request_body = LoginRequest,
    responses(
        (status = 200, description = "로그인함. 응답의 Set-Cookie에 세션 쿠키가 있다", body = SessionView,
            headers(("Set-Cookie" = String, description = "booking_admin_session=…; Path=/admin; HttpOnly; SameSite=Strict"))),
        (status = 400, description = "요청 본문 오류", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "아이디 또는 비밀번호가 틀림", body = Problem, content_type = "application/problem+json"),
    ),
)]
// `skip_all`: 인자(비밀번호가 든 본문)를 span에 남기지 않는다
#[tracing::instrument(name = "admin.login", skip_all)]
pub async fn login(
    State(state): State<ApiState>,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Json(request) = body?;
    let user = db::admin::find_user(&state.pool, &request.username).await?;
    let verified = admin_auth::verify_password(
        request.password,
        user.as_ref().map(|user| user.password_hash.clone()),
    )
    .await;
    let Some(user) = user.filter(|_| verified) else {
        // 시도한 이름은 남기지 않는다 (비밀번호를 이름 칸에 잘못 넣는 경우가 있다)
        tracing::warn!("관리자 로그인 실패");
        return Err(ApiError::LoginFailed);
    };
    let role: AdminRole = user.role.parse().map_err(ApiError::Internal)?;

    let token = admin_auth::generate_session_token();
    let ttl = TimeDelta::from_std(state.admin_auth.session_ttl)
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    let expires_at = db::normalize_timestamp(Utc::now() + ttl);
    db::admin::create_session(
        &state.pool,
        &admin_auth::hash_token(&token),
        user.id,
        expires_at,
    )
    .await?;
    let actor = format!("admin:{}", user.username);
    let mut conn = state.pool.acquire().await?;
    record_audit(
        &mut conn,
        &actor,
        "admin.login",
        None,
        serde_json::json!({}),
    )
    .await?;
    tracing::info!(username = %user.username, role = role.as_str(), "관리자가 로그인했다");

    let cookie = admin_auth::session_cookie(&token, &state.admin_auth);
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(SessionView {
            username: user.username,
            role,
            expires_at,
        }),
    ))
}

/// 로그아웃한다. 세션을 지우고 쿠키를 지우게 한다. 로그인하지 않았어도 204다.
#[utoipa::path(
    post,
    path = "/logout",
    operation_id = "admin_logout",
    tag = tags::ADMIN_AUTH,
    responses((status = 204, description = "로그아웃함 (쿠키를 지우는 Set-Cookie)")),
)]
#[tracing::instrument(name = "admin.logout", skip_all)]
pub async fn logout(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(token) = admin_auth::session_token(&headers) {
        let token_hash = admin_auth::hash_token(token);
        if let Some(user) = db::admin::find_session(&state.pool, &token_hash).await? {
            let mut conn = state.pool.acquire().await?;
            let actor = format!("admin:{}", user.username);
            record_audit(
                &mut conn,
                &actor,
                "admin.logout",
                None,
                serde_json::json!({}),
            )
            .await?;
        }
        db::admin::delete_session(&state.pool, &token_hash).await?;
    }
    let cookie = admin_auth::expired_cookie(&state.admin_auth);
    Ok((StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]))
}

/// 지금 로그인한 관리자. 로그인하지 않았으면 401 (관리자 화면이 로그인 화면을 보여 줄지 정한다)
#[utoipa::path(
    get,
    path = "/session",
    operation_id = "admin_session",
    tag = tags::ADMIN_AUTH,
    security(("admin_session" = [])),
    responses(
        (status = 200, description = "로그인한 관리자", body = SessionView),
        (status = 401, description = "로그인하지 않음 · 세션 만료", body = Problem, content_type = "application/problem+json"),
    ),
)]
pub async fn session(session: AdminSession) -> Json<SessionView> {
    Json(SessionView {
        username: session.username,
        role: session.role,
        expires_at: session.expires_at,
    })
}
