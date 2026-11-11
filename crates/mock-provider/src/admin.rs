//! 테스트 · 개발용 관리 API: 장애 설정(`/_chaos`), 내부 데이터 확인(`/_inspect`), 초기화(`/_reset`).
//! 장애 주입 미들웨어가 걸리지 않으므로 업체가 `outage` 상태여도 항상 응답한다.

use std::collections::BTreeMap;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use chrono::Utc;

use crate::{
    AppState, Provider,
    alpha::model::{ReservationList, ReservationQuery},
    beta::{BetaError, model::BookingList},
    chaos::{ChaosConfig, Preset},
    lock,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/_chaos", get(get_all_chaos))
        .route("/_chaos/{provider}", put(put_chaos))
        .route("/_chaos/{provider}/preset/{preset}", post(apply_preset))
        .route("/_reset", post(reset))
        .route(
            "/_inspect/{provider}/reservations",
            get(inspect_reservations),
        )
        .route(
            "/_inspect/{provider}/reservations/{id}/cancel",
            post(cancel_at_provider),
        )
}

async fn get_all_chaos(State(state): State<AppState>) -> Json<BTreeMap<Provider, ChaosConfig>> {
    Json(state.chaos.all())
}

async fn put_chaos(
    State(state): State<AppState>,
    Path(provider): Path<Provider>,
    Json(config): Json<ChaosConfig>,
) -> Result<Json<ChaosConfig>, (StatusCode, String)> {
    config
        .validate()
        .map_err(|reason| (StatusCode::BAD_REQUEST, reason))?;
    state.chaos.set(provider, config.clone());
    tracing::info!(?provider, ?config, "chaos config replaced");
    Ok(Json(config))
}

/// `Path((a, b))`: 경로 변수 두 개를 튜플로 받는다. 없는 프리셋 이름이면 axum이 400을 돌려준다.
async fn apply_preset(
    State(state): State<AppState>,
    Path((provider, preset)): Path<(Provider, Preset)>,
) -> Json<ChaosConfig> {
    let config = preset.config();
    state.chaos.set(provider, config.clone());
    tracing::info!(?provider, ?preset, "chaos preset applied");
    Json(config)
}

/// 모든 업체의 예약 데이터를 비우고(beta의 예약 번호도 1부터 다시) 장애 설정을 healthy로 되돌린다.
async fn reset(State(state): State<AppState>) -> StatusCode {
    *lock(&state.alpha) = crate::alpha::Store::default();
    *lock(&state.beta) = crate::beta::Store::default();
    state.chaos.reset_all(&Preset::Healthy.config());
    tracing::info!("mock provider state reset");
    StatusCode::NO_CONTENT
}

/// 저장된 예약 전체를 그 업체의 JSON 형식으로 돌려준다.
/// alpha는 `{"reservations": [...]}`, beta는 `{"bookings": [...]}`.
///
/// 업체마다 본문 타입이 달라서 `Json<T>` 하나로 적을 수 없다. 그래서 각 갈래에서 `Response`로 바꿔 돌려준다
/// (≈ ASP.NET Core에서 반환 타입을 `IActionResult`로 두는 것).
async fn inspect_reservations(
    State(state): State<AppState>,
    Path(provider): Path<Provider>,
) -> Response {
    match provider {
        Provider::Alpha => {
            let reservations = lock(&state.alpha).search(&ReservationQuery::default());
            Json(ReservationList { reservations }).into_response()
        }
        Provider::Beta => {
            let bookings = lock(&state.beta).all();
            Json(BookingList { bookings }).into_response()
        }
    }
}

/// 업체 쪽에서 임의로 예약을 취소한다 (대사 작업 테스트용). 취소한 예약을 그 업체의 JSON 형식으로 돌려준다.
async fn cancel_at_provider(
    State(state): State<AppState>,
    Path((provider, id)): Path<(Provider, String)>,
) -> Response {
    let now = Utc::now();
    match provider {
        // alpha의 취소는 멱등이라 이미 취소된 예약도 200. 없는 예약은 alpha 형식의 404.
        Provider::Alpha => match lock(&state.alpha).cancel(&id, now) {
            Ok(reservation) => Json(reservation).into_response(),
            Err(error) => error.into_response(),
        },
        Provider::Beta => match lock(&state.beta).cancel(&id, now) {
            Ok(booking) => Json(booking).into_response(),
            // beta 업체 API는 에러도 200으로 주지만, 관리용 API는 테스트가 알아보기 쉽게 상태 코드로도 알린다.
            // `(상태 코드, 응답)` 튜플은 본문은 그대로 두고 상태 코드만 바꾼다.
            Err(error) => (inspect_status(&error), error).into_response(),
        },
    }
}

/// 관리용 취소 API가 beta 에러에 붙이는 상태 코드
fn inspect_status(error: &BetaError) -> StatusCode {
    match error {
        BetaError::NotFound(_) => StatusCode::NOT_FOUND,
        BetaError::AlreadyCancelled => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    }
}
