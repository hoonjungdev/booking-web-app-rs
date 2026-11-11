//! 장애 주입이 가능한 가짜 외부 예약 업체 서버
//!
//! 진짜 외부 업체처럼 동작하도록 우리 다른 크레이트(domain 등)에는 의존하지 않는다.
//!
//! | 모듈 | 역할 |
//! |---|---|
//! | [`alpha`] | 모범적인 REST API를 흉내 내는 업체 (docs/04-api.md §4) |
//! | [`catalog`] | 업체가 가진 자원(매장)과 슬롯을 고정 규칙으로 생성 |
//! | [`chaos`] | 지연 · 오류 · 호출 제한 · 장애를 주입하는 설정과 미들웨어 |
//! | `admin` | `/_chaos`, `/_inspect`, `/_reset` 관리용 API |

// `mod` = 모듈 선언 (≈ C#의 namespace + 파일). `pub`이 붙으면 크레이트 밖(테스트 등)에서도 보인다.
mod admin;
pub mod alpha;
pub mod catalog;
pub mod chaos;
pub mod rate_limit;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::{Router, middleware};
use serde::{Deserialize, Serialize};
use telemetry::HttpTelemetryLayer;

/// 흉내 내는 외부 업체 종류. beta는 Phase 6에서 추가한다.
///
/// `#[derive(...)]`는 비교 · 해시 · 직렬화 같은 기본 구현을 컴파일러가 자동으로 만들어 주는 기능이다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Alpha,
}

impl Provider {
    pub const ALL: [Self; 1] = [Self::Alpha];
}

/// 모든 요청 핸들러가 공유하는 상태 (≈ ASP.NET Core에서 싱글턴으로 등록한 서비스들).
///
/// `Arc`는 여러 스레드가 함께 소유할 수 있는 참조 카운트 포인터이고,
/// `Mutex`는 한 번에 한 스레드만 안의 값을 바꾸게 하는 잠금이다 (≈ C#의 `lock`).
/// `#[derive(Clone)]`으로 복제해도 `Arc` 안의 실제 데이터는 하나만 있다.
#[derive(Clone)]
pub struct AppState {
    pub alpha: Arc<Mutex<alpha::Store>>,
    pub chaos: Arc<chaos::Registry>,
}

impl AppState {
    pub fn new(initial_chaos: &chaos::ChaosConfig) -> Self {
        Self {
            alpha: Arc::new(Mutex::new(alpha::Store::default())),
            chaos: Arc::new(chaos::Registry::new(initial_chaos)),
        }
    }
}

/// HTTP 라우터 전체를 조립한다. `main`과 테스트가 함께 쓴다.
pub fn app(state: AppState) -> Router {
    // 업체 API에만 장애 주입 미들웨어를 건다. `/_chaos` 같은 관리 API에는 걸지 않는다.
    let alpha_routes = alpha::routes().route_layer(middleware::from_fn_with_state(
        chaos::ChaosContext::new(state.chaos.clone(), Provider::Alpha),
        chaos::middleware,
    ));

    Router::new()
        .nest("/alpha/v1", alpha_routes)
        .merge(admin::routes())
        // 요청마다 서버 span을 연다. 호출한 쪽이 `traceparent` 헤더를 보냈으면 그 trace에 이어 붙으므로,
        // booking-server에서 시작한 trace 안에서 "업체 내부에서 무슨 일이 있었는지"까지 보인다.
        .layer(HttpTelemetryLayer::new())
        .with_state(state)
}

/// `Mutex`를 잠근다.
///
/// 잠금을 쥔 스레드가 패닉하면 Mutex가 "오염(poisoned)" 상태가 되어 `lock()`이 `Err`를 돌려준다.
/// mock 서버에서는 그래도 데이터를 계속 쓰는 편이 낫기 때문에 오염을 무시하고 안의 값을 꺼낸다.
/// (`unwrap()`을 쓰지 않는 이유: 프로젝트 규칙상 테스트 밖에서는 금지)
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
