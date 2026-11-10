//! 헬스 체크

use axum::{extract::State, http::StatusCode};

use super::ApiState;

/// 프로세스가 살아 있음 (liveness)
pub async fn healthz() -> &'static str {
    "ok"
}

/// 요청을 처리할 준비가 됨: DB에 연결할 수 있음 (readiness)
pub async fn readyz(State(state): State<ApiState>) -> (StatusCode, &'static str) {
    match sqlx::query("SELECT 1").execute(&state.pool).await {
        Ok(_) => (StatusCode::OK, "ok"),
        Err(error) => {
            tracing::warn!(%error, "readiness check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
        }
    }
}
