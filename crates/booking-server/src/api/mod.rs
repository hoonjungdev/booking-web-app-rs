//! 예약 API (docs/04-api.md §2)

mod bookings;
mod error;
mod health;

use axum::{
    Router,
    routing::{get, post},
};
use db::PgPool;
use telemetry::HttpTelemetryLayer;

pub use bookings::BookingResponse;
pub use error::ApiError;

/// API 핸들러가 공유하는 상태
#[derive(Clone)]
pub struct ApiState {
    pub pool: PgPool,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/v1/bookings", post(bookings::create))
        .route("/v1/bookings/{id}", get(bookings::get))
        // `layer`는 **이 줄보다 위에서** 등록한 라우트에만 걸린다 (없는 경로에 답하는 404도 포함).
        // 요청마다 서버 span을 열고, 지표를 기록하고, 응답에 `X-Trace-Id`를 붙인다.
        .layer(HttpTelemetryLayer::new())
        // 헬스 체크는 계측하지 않는다. 몇 초마다 불리는 요청이라 trace와 지표를 채워 실제 요청을 가린다.
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .with_state(state)
}
