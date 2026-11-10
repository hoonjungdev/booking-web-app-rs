//! `POST /v1/bookings`, `GET /v1/bookings/{id}`

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
use db::bookings::{AcceptOutcome, NewBooking};
use domain::{
    Booking, BookingId, BookingRequest, BookingStatus, Customer, ProviderId, confirmation_deadline,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{ApiError, ApiState};
use crate::auth::AuthenticatedClient;

const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;

/// 예약 생성 요청 본문. 모르는 필드가 있으면 400 (오타를 조용히 무시하지 않기 위해)
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBookingBody {
    pub provider: String,
    pub resource_id: String,
    pub slot_start: DateTime<Utc>,
    pub party_size: u32,
    pub customer: Customer,
}

/// 예약 응답. 고객 정보는 돌려주지 않는다 (개인정보 최소화).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        slot_start: body.slot_start,
        party_size: body.party_size,
        customer: body.customer,
    };
    let now = Utc::now();
    request
        .validate(now)
        .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;

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
    };

    let booking = match db::bookings::accept(&state.pool, &new).await? {
        AcceptOutcome::Created(booking) => {
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

    let location = format!("/v1/bookings/{}", booking.id);
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, location)],
        Json(BookingResponse::from(&booking)),
    ))
}

/// 자기 클라이언트의 예약을 조회한다. 다른 클라이언트의 예약이나 잘못된 ID는 모두 404.
pub async fn get(
    State(state): State<ApiState>,
    client: AuthenticatedClient,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<BookingResponse>, ApiError> {
    let Path(id) = id.map_err(|_| ApiError::BookingNotFound)?;
    let booking = db::bookings::find_for_client(&state.pool, client.id, BookingId::from_uuid(id))
        .await?
        .ok_or(ApiError::BookingNotFound)?;
    Ok(Json(BookingResponse::from(&booking)))
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
