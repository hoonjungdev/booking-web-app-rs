//! 테스트 · 개발용 관리 API: 장애 설정(`/_chaos`), 내부 데이터 확인(`/_inspect`), 초기화(`/_reset`).
//! 장애 주입 미들웨어가 걸리지 않으므로 업체가 `outage` 상태여도 항상 응답한다.

use std::collections::BTreeMap;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post, put},
};
use chrono::Utc;

use crate::{
    AppState, Provider,
    alpha::{
        AlphaError,
        model::{Reservation, ReservationList, ReservationQuery},
    },
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

/// 예약 데이터를 비우고 장애 설정을 healthy로 되돌린다.
async fn reset(State(state): State<AppState>) -> StatusCode {
    *lock(&state.alpha) = crate::alpha::Store::default();
    state.chaos.reset_all(&Preset::Healthy.config());
    tracing::info!("mock provider state reset");
    StatusCode::NO_CONTENT
}

async fn inspect_reservations(
    State(state): State<AppState>,
    Path(provider): Path<Provider>,
) -> Json<ReservationList> {
    let reservations = match provider {
        Provider::Alpha => lock(&state.alpha).search(&ReservationQuery::default()),
    };
    Json(ReservationList { reservations })
}

/// 업체 쪽에서 임의로 예약을 취소한다 (대사 작업 테스트용)
async fn cancel_at_provider(
    State(state): State<AppState>,
    Path((provider, id)): Path<(Provider, String)>,
) -> Result<Json<Reservation>, AlphaError> {
    let reservation = match provider {
        Provider::Alpha => lock(&state.alpha).cancel(&id, Utc::now())?,
    };
    Ok(Json(reservation))
}
