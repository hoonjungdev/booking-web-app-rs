//! `POST /v1/bookings`, `GET /v1/bookings/{id}`, `POST /v1/bookings/{id}/cancel`

use axum::{
    Json,
    extract::{
        Path, State,
        rejection::{JsonRejection, PathRejection},
    },
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use db::{
    bookings::{AcceptOutcome, NewBooking, Transitioned},
    jobs::{self, JobKind},
};
use domain::{
    Booking, BookingEvent, BookingId, BookingRequest, BookingStatus, Customer, ProviderId,
    confirmation_deadline,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{Span, field::Empty};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{ApiError, ApiState, Problem, docs::tags};
use crate::{auth::AuthenticatedClient, metrics, trace};

const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;

/// 예약 생성 요청 본문. 모르는 필드가 있으면 400 (오타를 조용히 무시하지 않기 위해)
///
/// `#[derive(ToSchema)]`: 이 구조체의 모양을 API 문서(OpenAPI 스키마)로 만든다. 필드의 문서 주석(`///`)이 설명이 된다.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateBookingBody {
    /// 업체 ID
    #[schema(example = "alpha")]
    pub provider: String,
    /// 업체 안의 예약 대상 (매장 등)
    #[schema(example = "store-001")]
    pub resource_id: String,
    /// 슬롯 시작 시각 (RFC 3339). 오프셋을 붙여도 된다 (`+09:00`)
    pub slot_start: DateTime<Utc>,
    /// 인원 (1~50)
    #[schema(minimum = 1, maximum = 50)]
    pub party_size: u32,
    /// 고객 정보. 저장하지만 응답 · 로그에는 내보내지 않는다
    pub customer: Customer,
}

/// 예약 응답. 고객 정보는 돌려주지 않는다 (개인정보 최소화).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BookingResponse {
    pub id: BookingId,
    pub status: BookingStatus,
    pub provider: ProviderId,
    pub resource_id: String,
    pub slot_start: DateTime<Utc>,
    pub party_size: u32,
    pub provider_booking_id: Option<String>,
    pub status_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<&Booking> for BookingResponse {
    fn from(booking: &Booking) -> Self {
        Self {
            id: booking.id,
            status: booking.status,
            provider: booking.request.provider.clone(),
            resource_id: booking.request.resource_id.clone(),
            slot_start: booking.request.slot_start,
            party_size: booking.request.party_size,
            provider_booking_id: booking.provider_booking_id.clone(),
            status_reason: booking.status_reason.clone(),
            created_at: booking.created_at,
            updated_at: booking.updated_at,
        }
    }
}

/// 예약을 접수한다. 업체 호출은 워커가 하므로 바로 `202 Accepted`를 돌려준다 (docs/decisions.md D-03).
///
/// `body`를 `Result<Json<_>, JsonRejection>`으로 받으면 본문 해석 실패를 직접 처리할 수 있다.
/// 그냥 `Json<_>`으로 받으면 axum이 일반 텍스트로 거절해 버려 Problem Details 형식을 지킬 수 없다.
///
/// `skip_all`이 중요하다: 인자(`body`)에는 고객 이름 · 전화번호가 들어 있어서 span에 기록하면 안 된다.
/// 예약 ID는 접수한 뒤에야 알 수 있으므로 `Empty`로 자리만 잡아 두고 나중에 채운다.
///
/// `#[utoipa::path]`: 이 핸들러의 경로 · 매개변수 · 응답을 API 문서에 적는다. `api::documented_routes`의
/// `routes!(bookings::create)`가 이 정보로 axum 라우트와 문서 경로를 함께 등록한다.
#[utoipa::path(
    post,
    path = "/v1/bookings",
    operation_id = "create_booking",
    tag = tags::BOOKINGS,
    security(("api_key" = [])),
    params(
        ("Idempotency-Key" = String, Header, description = "멱등키 (출력 가능한 ASCII 1~255자). 같은 키로 다시 보내면 같은 예약을 돌려준다"),
    ),
    request_body = CreateBookingBody,
    responses(
        (status = 202, description = "접수함 (업체 제출은 워커가 한다). 같은 키 + 같은 본문이면 기존 예약", body = BookingResponse,
            headers(("Location" = String, description = "예약 조회 경로"))),
        (status = 400, description = "입력 오류 · 지난 슬롯 · 멱등키 없음", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "API 키가 없거나 틀림", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "없는 업체", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "예약을 받지 않는 업체", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "같은 멱등키 + 다른 본문", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(
    name = "booking.accept",
    skip_all,
    fields(booking_id = Empty, provider = Empty)
)]
pub async fn create(
    State(state): State<ApiState>,
    client: AuthenticatedClient,
    headers: HeaderMap,
    body: Result<Json<CreateBookingBody>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let idempotency_key = idempotency_key(&headers)?;
    let Json(body) = body.map_err(|rejection| ApiError::InvalidRequest(rejection.body_text()))?;

    let provider = ProviderId::parse(&body.provider)
        .map_err(|_| ApiError::ProviderNotFound(body.provider.clone()))?;
    let request = BookingRequest {
        provider,
        resource_id: body.resource_id,
        // DB 정밀도(마이크로초)로 맞춘다. 그래야 요청 해시를 계산하는 값 = 저장되는 값 = 다시 읽는 값이 된다.
        slot_start: db::normalize_timestamp(body.slot_start),
        party_size: body.party_size,
        customer: body.customer,
    };
    let now = Utc::now();
    request
        .validate(now)
        .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
    Span::current().record("provider", request.provider.as_str());

    let provider = db::providers::find(&state.pool, &request.provider)
        .await?
        .ok_or_else(|| ApiError::ProviderNotFound(request.provider.to_string()))?;
    if !provider.enabled {
        return Err(ApiError::ProviderDisabled(provider.id.to_string()));
    }

    let new = NewBooking {
        id: BookingId::from_uuid(Uuid::now_v7()),
        client_id: client.id,
        idempotency_key,
        request_hash: request_hash(&request)?,
        deadline_at: confirmation_deadline(now, request.slot_start),
        request,
        // 이 요청의 trace를 상태 이력과 작업에 남긴다. 워커가 작업을 처리할 때 이 trace로 link를 건다.
        trace: trace::current(),
    };

    let outcome = db::bookings::accept(&state.pool, &new).await?;
    let booking = match outcome {
        AcceptOutcome::Created(booking) => {
            metrics::booking_accepted(&booking);
            tracing::info!(
                booking_id = %booking.id,
                client = %client.name,
                provider = %booking.request.provider,
                "booking accepted"
            );
            booking
        }
        AcceptOutcome::Existing {
            booking,
            same_request: true,
        } => booking,
        AcceptOutcome::Existing {
            same_request: false,
            ..
        } => return Err(ApiError::IdempotencyKeyReused),
    };
    // 같은 멱등키로 다시 온 요청이면 새로 만든 ID가 아니라 기존 예약의 ID다
    Span::current().record("booking_id", tracing::field::display(booking.id));

    let location = format!("/v1/bookings/{}", booking.id);
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, location)],
        Json(BookingResponse::from(&booking)),
    ))
}

/// 자기 클라이언트의 예약을 조회한다. 다른 클라이언트의 예약이나 잘못된 ID는 모두 404.
#[utoipa::path(
    get,
    path = "/v1/bookings/{id}",
    operation_id = "get_booking",
    tag = tags::BOOKINGS,
    security(("api_key" = [])),
    params(("id" = Uuid, Path, description = "예약 ID")),
    responses(
        (status = 200, description = "예약", body = BookingResponse),
        (status = 401, description = "API 키가 없거나 틀림", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "없거나 다른 클라이언트의 예약", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "booking.get", skip_all, fields(booking_id = Empty))]
pub async fn get(
    State(state): State<ApiState>,
    client: AuthenticatedClient,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<BookingResponse>, ApiError> {
    let Path(id) = id.map_err(|_| ApiError::BookingNotFound)?;
    Span::current().record("booking_id", tracing::field::display(id));
    let booking = db::bookings::find_for_client(&state.pool, client.id, BookingId::from_uuid(id))
        .await?
        .ok_or(ApiError::BookingNotFound)?;
    Ok(Json(BookingResponse::from(&booking)))
}

/// 확정된 예약의 취소를 요청한다. 업체 취소는 워커가 하므로 바로 `202 Accepted`를 돌려준다.
///
/// 취소는 원래 멱등이다: 이미 취소를 받은 예약(`cancel_requested` · `cancelled` · `cancel_failed`)이면 아무것도 바꾸지 않고
/// 지금 상태로 202를 돌려준다. 확정되지 않은 예약은 409다 (처리 중인 예약을 취소하면 경쟁 상태가 복잡해져 받지 않는다).
///
/// 예약 행을 잠그고 상태를 확인한 뒤, 상태 전이(`confirmed → cancel_requested`)와 `cancel_booking` 작업 등록을
/// 한 트랜잭션으로 저장한다. 두 취소 요청이 동시에 와도 작업은 하나만 생긴다.
#[utoipa::path(
    post,
    path = "/v1/bookings/{id}/cancel",
    operation_id = "cancel_booking",
    tag = tags::BOOKINGS,
    security(("api_key" = [])),
    params(("id" = Uuid, Path, description = "예약 ID")),
    responses(
        (status = 202, description = "취소를 접수함 (업체 취소는 워커가 한다). 이미 취소를 받은 예약이면 지금 상태", body = BookingResponse),
        (status = 401, description = "API 키가 없거나 틀림", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "없거나 다른 클라이언트의 예약", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "확정되지 않은 예약 (처리 중 · 거절 · 실패)", body = Problem, content_type = "application/problem+json"),
    ),
)]
#[tracing::instrument(name = "booking.cancel", skip_all, fields(booking_id = Empty))]
pub async fn cancel(
    State(state): State<ApiState>,
    client: AuthenticatedClient,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<(StatusCode, Json<BookingResponse>), ApiError> {
    let Path(id) = id.map_err(|_| ApiError::BookingNotFound)?;
    Span::current().record("booking_id", tracing::field::display(id));
    let id = BookingId::from_uuid(id);
    let trace = trace::current();

    let mut tx = state.pool.begin().await?;
    // 행을 잠그고 읽는다: 확인과 변경 사이에 다른 요청이 끼어들지 못한다
    let booking = db::bookings::get_for_update(&mut tx, id)
        .await?
        .filter(|booking| booking.client_id == client.id)
        .ok_or(ApiError::BookingNotFound)?;
    let requested = match booking.status {
        BookingStatus::Confirmed => {
            let transitioned = db::bookings::transition(
                &mut tx,
                id,
                BookingStatus::Confirmed,
                &BookingEvent::CancelRequested,
                "api",
                trace.trace_id.as_deref(),
            )
            .await?;
            if transitioned == Transitioned::Conflict {
                // 행을 잠갔으므로 일어나지 않는다
                return Err(ApiError::Conflict("그사이 예약 상태가 바뀌었습니다".into()));
            }
            jobs::enqueue(
                &mut tx,
                JobKind::CancelBooking,
                Some(id),
                jobs::DEFAULT_MAX_ATTEMPTS,
                trace.context.as_ref(),
            )
            .await?;
            true
        }
        // 이미 받은 취소: 그대로 돌려준다
        BookingStatus::CancelRequested | BookingStatus::Cancelled | BookingStatus::CancelFailed => {
            false
        }
        BookingStatus::Pending | BookingStatus::Unknown => {
            return Err(ApiError::Conflict(
                "아직 처리 중인 예약은 취소할 수 없습니다. 확정된 뒤에 다시 요청하세요".into(),
            ));
        }
        BookingStatus::Rejected | BookingStatus::Failed => {
            return Err(ApiError::Conflict(format!(
                "확정되지 않은 예약은 취소할 수 없습니다 (지금: {})",
                booking.status
            )));
        }
    };
    tx.commit().await?;

    if requested {
        // 지표는 커밋한 뒤에 센다
        metrics::booking_transitioned(&booking, BookingStatus::CancelRequested);
        tracing::info!(booking_id = %id, client = %client.name, "booking cancel requested");
    }
    let booking = db::bookings::get(&state.pool, id)
        .await?
        .ok_or(ApiError::BookingNotFound)?;
    Ok((StatusCode::ACCEPTED, Json(BookingResponse::from(&booking))))
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let key = headers
        .get(IDEMPOTENCY_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or(ApiError::IdempotencyKeyRequired)?;
    let valid = !key.is_empty()
        && key.len() <= MAX_IDEMPOTENCY_KEY_LEN
        && key.bytes().all(|b| b.is_ascii_graphic());
    if valid {
        Ok(key.to_owned())
    } else {
        Err(ApiError::IdempotencyKeyRequired)
    }
}

/// 요청 내용의 해시. 같은 멱등키로 다른 요청이 왔는지 비교하는 데 쓴다.
///
/// 검증을 마친 `BookingRequest`를 직렬화해서 해시하므로, `"19:00+09:00"`과 `"10:00Z"`처럼
/// 표기만 다르고 뜻이 같은 요청은 같은 해시가 된다.
fn request_hash(request: &BookingRequest) -> Result<Vec<u8>, ApiError> {
    let canonical =
        serde_json::to_vec(request).map_err(|error| ApiError::Internal(error.to_string()))?;
    Ok(Sha256::digest(&canonical).to_vec())
}
