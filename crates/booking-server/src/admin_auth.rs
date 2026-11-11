//! 관리자 인증 · 권한 (docs/06-admin-frontend.md §6, docs/decisions.md D-36)
//!
//! ```text
//! POST /admin/api/login {username, password}
//!   └─ argon2로 비밀번호 확인 → 256비트 무작위 토큰 발급
//!      DB(admin_sessions)에는 토큰의 SHA-256 해시만, 브라우저에는 쿠키로 토큰 원문
//!      Set-Cookie: booking_admin_session=<토큰>; Path=/admin; HttpOnly; SameSite=Strict; Secure
//! 이후 관리자 API 요청 ── 쿠키의 토큰을 해시해 세션을 찾는다 → 역할(viewer < operator < admin) 확인
//! ```
//!
//! 비밀번호와 API 키(세션 토큰)는 해시 방법이 다르다 (D-14): 비밀번호는 사람이 정해 짧고 추측하기 쉬워 느린 해시(argon2)를,
//! 세션 토큰은 서버가 만든 256비트 무작위 값이라 빠른 해시(SHA-256)를 쓴다.
//!
//! 핸들러는 매개변수 타입으로 필요한 권한을 밝힌다 (≈ ASP.NET Core의 `[Authorize(Roles = "...")]`).
//! - [`AdminSession`] — 로그인했으면 누구나 (viewer 이상, 조회)
//! - [`RequireOperator`] — operator 이상 (재처리 · 서킷 조작 · 장애 주입 · 대사)
//! - [`RequireAdmin`] — admin만 (정책 변경)
//!
//! 권한 확인을 핸들러 본문에서 빠뜨리는 실수를 막기 위해, 확인을 추출기(타입)에 넣었다. 매개변수에 타입이 있으면
//! axum이 핸들러를 부르기 전에 확인하고, 실패하면 핸들러는 실행되지 않는다.

use std::{str::FromStr, time::Duration};

use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, HeaderValue, header, request::Parts},
};
use chrono::{DateTime, Utc};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    api::{ApiError, ApiState},
    auth::hash_api_key,
};

/// 세션 쿠키 이름
pub const SESSION_COOKIE: &str = "booking_admin_session";
/// 쿠키가 적용되는 경로. 관리자 화면(`/admin`)과 관리자 API(`/admin/api`)에만 실려 간다.
const COOKIE_PATH: &str = "/admin";

/// 관리자 역할. 선언한 순서대로 권한이 커진다 (`derive(PartialOrd, Ord)`: 앞에 선언한 값이 작다).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdminRole {
    /// 조회만
    Viewer,
    /// 재처리 · 서킷 조작 · 장애 주입 · 대사
    Operator,
    /// 정책 변경까지
    Admin,
}

impl AdminRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Operator => "operator",
            Self::Admin => "admin",
        }
    }
}

impl FromStr for AdminRole {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        [Self::Viewer, Self::Operator, Self::Admin]
            .into_iter()
            .find(|role| role.as_str() == value)
            .ok_or_else(|| format!("알 수 없는 관리자 역할: {value} (viewer | operator | admin)"))
    }
}

/// 세션 설정
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminAuthConfig {
    /// 로그인 세션이 유지되는 시간
    pub session_ttl: Duration,
    /// 쿠키에 `Secure`를 붙인다 (HTTPS에서만 보냄). 브라우저는 `localhost` · `127.0.0.1`을 안전한 곳으로 봐서
    /// 로컬 http에서도 대부분 동작한다. 그렇지 않은 브라우저로 로컬에서 쓸 때만 끈다.
    pub cookie_secure: bool,
}

impl Default for AdminAuthConfig {
    fn default() -> Self {
        Self {
            session_ttl: Duration::from_secs(12 * 3600),
            cookie_secure: true,
        }
    }
}

/// 로그인한 관리자 (viewer 이상이면 누구나).
///
/// 핸들러 매개변수에 이 타입을 넣으면, axum이 핸들러를 부르기 전에 쿠키의 세션을 확인한다. 없거나 만료됐으면 401.
#[derive(Debug, Clone)]
pub struct AdminSession {
    pub user_id: Uuid,
    pub username: String,
    pub role: AdminRole,
    pub expires_at: DateTime<Utc>,
}

impl AdminSession {
    /// `booking_events.actor` · 감사 로그에 남기는 이름 (`admin:<이름>`)
    pub fn actor(&self) -> String {
        format!("admin:{}", self.username)
    }

    fn require(self, role: AdminRole) -> Result<Self, ApiError> {
        if self.role >= role {
            Ok(self)
        } else {
            Err(ApiError::Forbidden(role))
        }
    }
}

impl FromRequestParts<ApiState> for AdminSession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        let token = session_token(&parts.headers).ok_or(ApiError::LoginRequired)?;
        let user = db::admin::find_session(&state.pool, &hash_token(token))
            .await?
            .ok_or(ApiError::LoginRequired)?;
        let role = user
            .role
            .parse()
            .map_err(|error: String| ApiError::Internal(error))?;
        Ok(Self {
            user_id: user.user_id,
            username: user.username,
            role,
            expires_at: user.expires_at,
        })
    }
}

/// operator 이상만 (재처리 · 서킷 조작 · 장애 주입 · 대사). 권한이 모자라면 403.
#[derive(Debug, Clone)]
pub struct RequireOperator(pub AdminSession);

impl FromRequestParts<ApiState> for RequireOperator {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        let session = AdminSession::from_request_parts(parts, state).await?;
        session.require(AdminRole::Operator).map(Self)
    }
}

/// admin만 (정책 변경). 권한이 모자라면 403.
#[derive(Debug, Clone)]
pub struct RequireAdmin(pub AdminSession);

impl FromRequestParts<ApiState> for RequireAdmin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        let session = AdminSession::from_request_parts(parts, state).await?;
        session.require(AdminRole::Admin).map(Self)
    }
}

/// 요청의 `Cookie` 헤더에서 세션 토큰을 꺼낸다 (`a=1; booking_admin_session=…; b=2`)
pub fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, token)| token)
        .filter(|token| !token.is_empty())
}

/// 새 세션 토큰: 무작위 32바이트(256비트)를 16진수로 적은 것 (API 키와 같은 강도)
pub fn generate_session_token() -> String {
    let bytes: [u8; 32] = rand::rng().random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 세션 토큰의 해시 (DB에는 이 값만 저장한다)
pub fn hash_token(token: &str) -> String {
    hash_api_key(token)
}

/// 로그인 쿠키 (`Set-Cookie` 헤더 값)
pub fn session_cookie(token: &str, config: &AdminAuthConfig) -> HeaderValue {
    cookie_header(token, config.session_ttl.as_secs(), config.cookie_secure)
}

/// 로그아웃 쿠키: 같은 이름 · 경로에 빈 값과 `Max-Age=0`을 보내면 브라우저가 쿠키를 지운다
pub fn expired_cookie(config: &AdminAuthConfig) -> HeaderValue {
    cookie_header("", 0, config.cookie_secure)
}

fn cookie_header(token: &str, max_age_secs: u64, secure: bool) -> HeaderValue {
    // HttpOnly: 화면의 JavaScript가 읽지 못한다 (XSS로 세션이 새지 않게)
    // SameSite=Strict: 다른 사이트에서 시작한 요청에는 실리지 않는다 (CSRF 방어)
    let secure = if secure { "; Secure" } else { "" };
    let cookie = format!(
        "{SESSION_COOKIE}={token}; Path={COOKIE_PATH}; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}{secure}"
    );
    // 토큰은 16진수라 헤더에 넣을 수 없는 글자가 없다
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// 새 관리자의 비밀번호: 무작위 18바이트를 16진수로 (36자). `create-admin` 명령이 한 번만 보여 준다.
pub fn generate_password() -> String {
    let bytes: [u8; 18] = rand::rng().random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 비밀번호를 argon2로 해시한다 (솔트는 무작위로 만든다).
///
/// argon2는 일부러 느리게(수십 ms) 만든 계산이다. async 태스크에서 그대로 돌리면 그동안 그 스레드의 다른 태스크가 멈추므로,
/// CPU 작업용 스레드 풀(`spawn_blocking`)에 맡기고 결과를 기다린다 (≈ C#의 `Task.Run(() => …)`).
pub async fn hash_password(password: String) -> Result<String, ApiError> {
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|hash| hash.to_string())
            .map_err(|error| ApiError::Internal(format!("비밀번호를 해시할 수 없습니다: {error}")))
    })
    .await
    .map_err(|error| ApiError::Internal(error.to_string()))?
}

/// 비밀번호가 해시와 맞는가. 계정이 없으면(`hash`가 `None`) 가짜 해시로 같은 계산을 하고 `false`.
///
/// 계정이 없을 때 바로 돌려주면 응답 시간 차이로 "이 이름의 계정이 있는지"를 알아낼 수 있다 (사용자 이름 추측).
pub async fn verify_password(password: String, hash: Option<String>) -> bool {
    let exists = hash.is_some();
    let hash = hash.unwrap_or_else(|| DUMMY_HASH.to_owned());
    let verified = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).is_ok_and(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
    })
    .await
    .unwrap_or(false);
    exists && verified
}

/// 계정이 없을 때 시간을 맞추려고 확인해 보는 가짜 해시. 형식만 맞고 어떤 비밀번호와도 맞지 않는다.
/// 매개변수(m=19456, t=2, p=1)가 argon2 기본 설정과 같아 실제 확인과 비슷한 시간이 걸린다.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0c2FsdA$Gb3OdXlpPr5ftB7ixe/vPzcVsHEbvrFkSRbzeTV2pGg";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 역할은_viewer_operator_admin_순으로_커진다() {
        assert!(AdminRole::Viewer < AdminRole::Operator);
        assert!(AdminRole::Operator < AdminRole::Admin);
        assert_eq!("operator".parse::<AdminRole>(), Ok(AdminRole::Operator));
        assert!("root".parse::<AdminRole>().is_err());
    }

    #[test]
    fn 쿠키_헤더에서_세션_토큰을_꺼낸다() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; booking_admin_session=abc123; other=1"),
        );
        assert_eq!(session_token(&headers), Some("abc123"));

        let mut empty = HeaderMap::new();
        empty.insert(
            header::COOKIE,
            HeaderValue::from_static("booking_admin_session="),
        );
        assert_eq!(session_token(&empty), None);
        assert_eq!(session_token(&HeaderMap::new()), None);
    }

    #[test]
    fn 세션_쿠키는_스크립트가_읽지_못하고_다른_사이트_요청에_실리지_않는다() {
        let config = AdminAuthConfig::default();
        let cookie = session_cookie("abc", &config);
        let cookie = cookie.to_str().unwrap();
        for part in [
            "booking_admin_session=abc",
            "Path=/admin",
            "HttpOnly",
            "SameSite=Strict",
            "Max-Age=43200",
            "Secure",
        ] {
            assert!(cookie.contains(part), "{cookie}에 {part}가 없다");
        }
        let cleared = expired_cookie(&AdminAuthConfig {
            cookie_secure: false,
            ..config
        });
        let cleared = cleared.to_str().unwrap();
        assert!(cleared.contains("booking_admin_session=;"));
        assert!(cleared.contains("Max-Age=0"));
        assert!(!cleared.contains("Secure"));
    }

    #[tokio::test]
    async fn 비밀번호는_해시와_맞을_때만_통과한다() {
        let hash = hash_password("correct horse".into()).await.unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(!hash.contains("correct horse"));
        assert!(verify_password("correct horse".into(), Some(hash.clone())).await);
        assert!(!verify_password("wrong".into(), Some(hash)).await);
        assert!(!verify_password("not-a-password".into(), None).await);
        assert!(!verify_password("x".into(), Some("not a phc string".into())).await);
    }

    #[test]
    fn 시간을_맞추는_가짜_해시는_올바른_형식이다() {
        assert!(PasswordHash::new(DUMMY_HASH).is_ok());
    }
}
