//! 관리자: 화면이 알아야 할 서버 설정

use axum::{Json, extract::State};
use serde::Serialize;
use utoipa::ToSchema;

use crate::api::{ApiState, docs::tags};

/// 관리자 화면 설정. 화면이 처음 열릴 때 읽어, 쓸 수 없는 기능은 숨기거나 이유를 보여 준다.
#[derive(Debug, Serialize, ToSchema)]
pub struct AdminInfo {
    /// 서버 버전
    pub version: &'static str,
    /// trace ID를 Grafana(Tempo) 링크로 보여 줄 때 쓸 주소. 없으면 링크를 만들지 않는다
    pub grafana_url: Option<String>,
    /// 장애 주입을 쓸 수 있는가 (`BOOKING_CHAOS_URL`을 설정했는가)
    pub chaos_enabled: bool,
    /// 업체 런타임 상태(서킷 등)를 보고 조작할 수 있는가 (워커와 같은 프로세스인가)
    pub provider_runtime: bool,
}

/// 관리자 화면 설정
#[utoipa::path(
    get,
    path = "/info",
    operation_id = "admin_info",
    tag = tags::ADMIN,
    responses((status = 200, description = "관리자 화면 설정", body = AdminInfo)),
)]
pub async fn admin_info(State(state): State<ApiState>) -> Json<AdminInfo> {
    Json(AdminInfo {
        version: env!("CARGO_PKG_VERSION"),
        grafana_url: state.grafana_url.clone(),
        chaos_enabled: state.chaos.is_some(),
        provider_runtime: state.providers.is_some(),
    })
}
