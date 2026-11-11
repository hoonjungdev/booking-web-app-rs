//! beta 업체: 까다로운 레거시 API (docs/04-api.md §4)
//!
//! alpha와 일부러 다르게 만들었다. 어댑터가 이 차이를 흡수해야 한다.
//!
//! | | alpha | beta |
//! |---|---|---|
//! | 필드명 | snake_case | camelCase |
//! | 시각 | RFC 3339 UTC | 오프셋 없는 한국 시간 `yyyyMMddHHmm` |
//! | 업무 에러 | 4xx + `{"error": {...}}` | **HTTP 200** + `{"ok": false, "code": ...}` |
//! | 멱등키 | `Idempotency-Key` 지원 | **없음** (같은 요청을 두 번 보내면 예약 두 개) |
//! | 호출 한도 초과 | 429 + `Retry-After` | **503** + `Retry-After` (chaos 미들웨어가 처리) |
//! | `X-RateLimit-*` 헤더 | 있음 | 없음 |
//! | 취소 | `DELETE`, 멱등 | `POST .../cancel`, 두 번째는 `ALREADY_CANCELLED` |

mod error;
pub mod model;
mod store;
pub mod time;

use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    routing::{get, post},
};
use chrono::{NaiveDate, Utc};
use serde::Serialize;

pub use error::BetaError;
pub use store::Store;

use self::model::{BookRequest, BookingList, BookingResult, BookingsQuery, ShopSlots, SlotsQuery};
use crate::{AppState, lock};

/// `/beta/api` 아래에 붙는 라우트
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/slots", get(slots))
        .route("/book", post(book))
        .route("/book/by-ref/{external_ref}", get(by_ref))
        .route("/book/{booking_no}/cancel", post(cancel))
        .route("/bookings", get(bookings))
}

/// 성공 응답: `{"ok": true, ...본문 필드}`.
///
/// `#[serde(flatten)]`은 `body`를 `{"body": {...}}`로 감싸지 않고 그 필드들을 한 단계 위로 펼쳐 넣는다.
/// `T`는 제네릭 타입 매개변수라서 (≈ C#의 제네릭 클래스) 어떤 본문 타입이든 감쌀 수 있다.
#[derive(Debug, Serialize)]
struct Success<T> {
    ok: bool,
    #[serde(flatten)]
    body: T,
}

/// 핸들러의 반환 타입. 성공이든 실패든 HTTP 상태는 200이고 본문의 `ok`로 구분한다.
type Reply<T> = Result<Json<Success<T>>, BetaError>;

fn success<T>(body: T) -> Reply<T> {
    Ok(Json(Success { ok: true, body }))
}

/// 쿼리 · 본문 · 경로를 `Result<…, …Rejection>`으로 받는 이유: 그냥 `Query<T>`로 받으면 해석에 실패했을 때
/// axum이 400 + 일반 텍스트로 대신 답해 버린다. 이 업체는 그런 경우에도 HTTP 200 + `BAD_REQUEST`로 답한다.
/// `query?`의 `?`는 `From<QueryRejection> for BetaError` 구현으로 에러를 바꿔 돌려준다 (error.rs).
async fn slots(
    State(state): State<AppState>,
    query: Result<Query<SlotsQuery>, QueryRejection>,
) -> Reply<ShopSlots> {
    let Query(query) = query?;
    let shop_code = required(query.shop_code, "shopCode")?;
    let day = required_day(query.day, "day")?;
    let slots = lock(&state.beta).slots(&shop_code, day)?;
    success(ShopSlots {
        shop_code,
        day,
        slots,
    })
}

async fn book(
    State(state): State<AppState>,
    body: Result<Json<BookRequest>, JsonRejection>,
) -> Reply<BookingResult> {
    let Json(request) = body?;
    let booking = lock(&state.beta).create(request, Utc::now())?;
    success(BookingResult {
        booking_no: booking.booking_no,
        status: booking.status,
    })
}

/// 참조값으로 예약을 찾는다. 같은 참조값으로 여러 번 예약했으면 모두 돌려준다 (만든 순서).
async fn by_ref(
    State(state): State<AppState>,
    external_ref: Result<Path<String>, PathRejection>,
) -> Reply<BookingList> {
    let Path(external_ref) = external_ref?;
    let bookings = lock(&state.beta).by_ref(&external_ref);
    if bookings.is_empty() {
        return Err(BetaError::NotFound(external_ref));
    }
    success(BookingList { bookings })
}

async fn bookings(
    State(state): State<AppState>,
    query: Result<Query<BookingsQuery>, QueryRejection>,
) -> Reply<BookingList> {
    let Query(query) = query?;
    let from_day = required_day(query.from_day, "fromDay")?;
    let to_day = required_day(query.to_day, "toDay")?;
    if from_day > to_day {
        return Err(BetaError::BadRequest(
            "fromDay는 toDay보다 늦을 수 없습니다".into(),
        ));
    }
    success(BookingList {
        bookings: lock(&state.beta).by_days(from_day, to_day),
    })
}

async fn cancel(
    State(state): State<AppState>,
    booking_no: Result<Path<String>, PathRejection>,
) -> Reply<BookingResult> {
    let Path(booking_no) = booking_no?;
    let booking = lock(&state.beta).cancel(&booking_no, Utc::now())?;
    success(BookingResult {
        booking_no: booking.booking_no,
        status: booking.status,
    })
}

/// 꼭 있어야 하는 쿼리 값. 없거나 비어 있으면 `BAD_REQUEST`
fn required(value: Option<String>, name: &str) -> Result<String, BetaError> {
    value
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| BetaError::BadRequest(format!("{name} 값이 필요합니다")))
}

/// 꼭 있어야 하는 `yyyyMMdd` 날짜 쿼리 값
fn required_day(value: Option<String>, name: &str) -> Result<NaiveDate, BetaError> {
    let text = required(value, name)?;
    time::parse_day(&text)
        .ok_or_else(|| BetaError::BadRequest(format!("{name} 값은 yyyyMMdd 형식이어야 합니다")))
}
