//! 예약 API (docs/04-api.md §2)

mod bookings;
mod error;
mod health;

use axum::{
    Router,
    routing::{get, post},
};
use db::PgPool;
use tower_http::trace::TraceLayer;

pub use bookings::BookingResponse;
pub use error::ApiError;

/// API 핸들러가 공유하는 상태
#[derive(Clone)]
pub struct ApiState {
    pub pool: PgPool,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .route("/v1/bookings", post(bookings::create))
        .route("/v1/bookings/{id}", get(bookings::get))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
