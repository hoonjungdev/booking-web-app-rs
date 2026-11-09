//! alpha 업체: 모범적인 REST API (docs/04-api.md §4)
//!
//! - 시각은 RFC 3339 UTC
//! - `POST /reservations`는 `Idempotency-Key` 헤더를 지원한다
//! - 호출 한도를 넘으면 429 + `Retry-After` (chaos 미들웨어가 처리)

mod error;
pub mod model;
mod store;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use chrono::Utc;

pub use error::AlphaError;
pub use store::{CreateOutcome, Store};

use self::model::{
    Availability, AvailabilityQuery, CreateReservationRequest, Reservation, ReservationList,
    ReservationQuery, ResourceInfo, ResourceList,
};
use crate::{AppState, catalog, lock};

const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// `/alpha/v1` 아래에 붙는 라우트
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/resources", get(list_resources))
        .route("/availability", get(availability))
        .route(
            "/reservations",
            get(search_reservations).post(create_reservation),
        )
        .route(
            "/reservations/{id}",
            get(get_reservation).delete(cancel_reservation),
        )
}

async fn list_resources() -> Json<ResourceList> {
    let resources = catalog::RESOURCES
        .iter()
        .map(|r| ResourceInfo {
            id: r.id.to_owned(),
            name: r.name.to_owned(),
            timezone: "Asia/Seoul".to_owned(),
        })
        .collect();
    Json(ResourceList { resources })
}

/// axum은 핸들러 매개변수 타입을 보고 요청에서 값을 꺼내 준다 (≈ ASP.NET Core 모델 바인딩).
/// `Query<T>`는 쿼리 문자열, `Path<T>`는 경로, `Json<T>`는 본문, `State<T>`는 공유 상태.
async fn availability(
    State(state): State<AppState>,
    Query(query): Query<AvailabilityQuery>,
) -> Result<Json<Availability>, AlphaError> {
    let slots = lock(&state.alpha).availability(&query.resource_id, query.date)?;
    Ok(Json(Availability {
        resource_id: query.resource_id,
        date: query.date,
        slots,
    }))
}

async fn create_reservation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateReservationRequest>,
) -> Result<(StatusCode, Json<Reservation>), AlphaError> {
    let key = match headers.get(IDEMPOTENCY_KEY) {
        None => None,
        Some(value) => Some(
            value
                .to_str()
                .ok()
                .filter(|k| !k.is_empty() && k.len() <= 255)
                .ok_or_else(|| {
                    AlphaError::InvalidRequest("Idempotency-Key는 1~255자 ASCII여야 합니다".into())
                })?,
        ),
    };

    let outcome = lock(&state.alpha).create(request, key, Utc::now())?;
    Ok(match outcome {
        CreateOutcome::Created(r) => (StatusCode::CREATED, Json(r)),
        CreateOutcome::Replayed(r) => (StatusCode::OK, Json(r)),
    })
}

async fn get_reservation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Reservation>, AlphaError> {
    Ok(Json(lock(&state.alpha).get(&id)?))
}

async fn search_reservations(
    State(state): State<AppState>,
    Query(query): Query<ReservationQuery>,
) -> Json<ReservationList> {
    Json(ReservationList {
        reservations: lock(&state.alpha).search(&query),
    })
}

async fn cancel_reservation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Reservation>, AlphaError> {
    Ok(Json(lock(&state.alpha).cancel(&id, Utc::now())?))
}
