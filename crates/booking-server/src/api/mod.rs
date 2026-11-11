//! HTTP API: 예약 API(`/v1`, docs/04-api.md §2)와 관리자 API(`/admin/api`, §3), API 문서(`/openapi.json`, `/docs`)
//!
//! 라우트는 utoipa-axum의 [`OpenApiRouter`]로 등록한다. 핸들러에 붙인 `#[utoipa::path(...)]`가 경로 · 매개변수 · 응답을
//! 적어 두면, `routes!(핸들러)` 한 줄이 axum 라우트 등록과 OpenAPI 문서의 경로 추가를 함께 한다.
//! 경로 문자열이 한곳에만 있으므로 라우트와 문서가 어긋나지 않는다 (≈ ASP.NET Core에서 Swashbuckle이 컨트롤러 라우트로 문서를 만드는 것).
//!
//! 관리자 웹(React)은 이 문서에서 TypeScript 타입을 만든다 (docs/06-admin-frontend.md §4):
//! `booking-server openapi > web/admin/openapi.json` → `pnpm gen:api`.

pub mod admin;
mod bookings;
mod docs;
mod error;
mod health;
mod providers;

use std::{sync::Arc, time::Duration};

use axum::{Router, routing::get};
use db::PgPool;
use telemetry::HttpTelemetryLayer;
use utoipa::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};
use utoipa_swagger_ui::SwaggerUi;

pub use bookings::BookingResponse;
pub use error::{ApiError, Problem};

use crate::{
    admin_auth::AdminAuthConfig, availability::AvailabilityCache, chaos::ChaosClient,
    registry::ProviderRegistry,
};

/// API 핸들러가 공유하는 상태 (≈ ASP.NET Core에서 DI 컨테이너에 등록한 서비스들. 여기서는 명시적인 구조체다)
///
/// `#[derive(Clone)]`: axum은 요청마다 상태를 복제해 핸들러에 넘긴다. 안의 값은 모두 `Arc`나 연결 풀이라 복제해도
/// 실제 데이터는 하나다.
#[derive(Clone)]
pub struct ApiState {
    pub pool: PgPool,
    /// 업체 파이프라인 (서킷 · rate limit · bulkhead의 지금 상태). 워커와 같은 프로세스에서 돌 때만 있다
    /// (`--role api`면 `None`. 서킷 상태는 업체를 부르는 프로세스의 메모리에 있다: docs/decisions.md D-10).
    pub providers: Option<Arc<ProviderRegistry>>,
    /// mock 업체 장애 주입 API (개발 환경 전용, `BOOKING_CHAOS_URL`). 설정하지 않으면 `None`
    pub chaos: Option<ChaosClient>,
    /// 관리자 화면의 trace 링크에 쓸 Grafana 주소
    pub grafana_url: Option<String>,
    /// 빈 시간 조회 캐시 (TTL + stale-if-error)
    pub availability: AvailabilityCache,
    /// 관리자 로그인 세션 설정 (유지 시간, 쿠키의 `Secure`)
    pub admin_auth: AdminAuthConfig,
    /// 설정된 워커 lease (`BOOKING_WORKER_LEASE_SECS`). 정책을 바꿀 때 업체 호출 한 번이 lease보다 길어지지 않는지 검사한다
    /// (길면 호출 도중 다른 워커가 같은 작업을 가져간다). 워커가 없는 프로세스(`--role api`)에서도 같은 값으로 검사한다.
    /// `None`이면 검사하지 않는다 (테스트용).
    pub worker_lease: Option<Duration>,
}

impl ApiState {
    /// DB만 있는 상태. 테스트와 `--role api`에서 쓰고, 나머지는 필요한 만큼 채운다.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            providers: None,
            chaos: None,
            grafana_url: None,
            availability: AvailabilityCache::new(),
            admin_auth: AdminAuthConfig::default(),
            worker_lease: None,
        }
    }
}

/// 문서에 오르는 모든 라우트. 상태(`ApiState`)는 아직 넣지 않은 채로 둔다 (문서만 뽑을 때는 상태가 필요 없다).
fn documented_routes() -> OpenApiRouter<ApiState> {
    OpenApiRouter::with_openapi(docs::ApiDoc::openapi())
        .routes(routes!(bookings::create))
        .routes(routes!(bookings::get))
        .routes(routes!(bookings::cancel))
        .routes(routes!(providers::list_providers))
        .routes(routes!(providers::availability))
        .nest("/admin/api", admin::routes())
}

/// API 문서 (OpenAPI 3.1). `booking-server openapi` 명령이 이 값을 JSON으로 출력한다.
pub fn openapi() -> utoipa::openapi::OpenApi {
    documented_routes().into_openapi()
}

pub fn router(state: ApiState) -> Router {
    let (routes, openapi) = documented_routes().split_for_parts();
    routes
        // `layer`는 **이 줄보다 위에서** 등록한 라우트에만 걸린다 (없는 경로에 답하는 404도 포함).
        // 요청마다 서버 span을 열고, 지표를 기록하고, 응답에 `X-Trace-Id`를 붙인다.
        .layer(HttpTelemetryLayer::new())
        // 아래는 계측하지 않는다.
        // 관리자 API에 없는 경로는 JSON 404로 답한다. 이것이 없으면 `/admin/{*rest}`(관리자 웹 화면)가 받아 HTML을 돌려준다.
        .route("/admin/api/{*rest}", axum::routing::any(admin::not_found))
        // API 문서: `/openapi.json`(문서 원본), `/docs`(Swagger UI)
        .merge(SwaggerUi::new("/docs").url("/openapi.json", openapi))
        // 헬스 체크는 몇 초마다 불리는 요청이라 trace와 지표를 채워 실제 요청을 가린다 (docs/decisions.md D-24).
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .with_state(state)
}
