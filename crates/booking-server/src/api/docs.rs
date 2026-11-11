//! OpenAPI 문서의 머리말: 제목, 설명, 태그, 인증 방식
//!
//! 경로와 스키마는 핸들러의 `#[utoipa::path]`와 `#[derive(ToSchema)]`에서 모인다 (`api::documented_routes`).

use utoipa::{
    Modify, OpenApi,
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
};

use crate::chaos::ChaosPreset;

/// 예약 API의 인증 방식 이름. 핸들러의 `security(("api_key" = []))`가 이 이름을 가리킨다.
pub const API_KEY_SCHEME: &str = "api_key";

/// 태그: Swagger UI에서 경로를 묶어 보여 주는 단위
pub mod tags {
    pub const BOOKINGS: &str = "bookings";
    pub const ADMIN_BOOKINGS: &str = "admin: bookings";
    pub const ADMIN_JOBS: &str = "admin: jobs";
    pub const ADMIN_PROVIDERS: &str = "admin: providers";
    pub const ADMIN_STATS: &str = "admin: stats";
    pub const ADMIN_CHAOS: &str = "admin: chaos";
    pub const ADMIN: &str = "admin";
}

/// `#[derive(OpenApi)]`: 속성에 적은 내용으로 OpenAPI 문서의 뼈대를 만든다.
/// `modifiers`: 속성만으로 표현하기 어려운 부분(인증 방식)을 코드로 덧붙인다.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Booking Gateway API",
        description = "외부 예약 업체 연동 게이트웨이.\n\n\
            - `/v1`: 예약 API (클라이언트용, `X-Api-Key` 인증)\n\
            - `/admin/api`: 관리자 API (Phase 6 전까지는 로컬 전용, 인증 없음)\n\n\
            오류는 RFC 9457 Problem Details(`application/problem+json`)로 돌려준다. \
            응답 헤더 `X-Trace-Id`로 Grafana에서 요청의 처리 과정을 찾을 수 있다.",
    ),
    tags(
        (name = "bookings", description = "예약 API (클라이언트용)"),
        (name = "admin: bookings", description = "관리자: 예약 조회 · 재시도"),
        (name = "admin: jobs", description = "관리자: 작업 큐"),
        (name = "admin: providers", description = "관리자: 업체 설정 · 런타임 상태 · 서킷 조작"),
        (name = "admin: stats", description = "관리자: 업체 호출 통계 (provider_calls)"),
        (name = "admin: chaos", description = "관리자: mock 업체 장애 주입 중계 (개발 환경 전용)"),
        (name = "admin", description = "관리자 화면 설정"),
    ),
    // 경로 매개변수로만 쓰는 타입은 저절로 등록되지 않아 직접 적는다 (`/admin/api/chaos/{provider}/preset/{name}`)
    components(schemas(ChaosPreset)),
    modifiers(&ApiKeySecurity),
)]
pub struct ApiDoc;

/// 예약 API의 `X-Api-Key` 헤더 인증을 문서에 등록한다
struct ApiKeySecurity;

impl Modify for ApiKeySecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            API_KEY_SCHEME,
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "X-Api-Key",
                "`booking-server create-client`로 발급한 API 키",
            ))),
        );
    }
}
