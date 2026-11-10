//! alpha 업체 어댑터 (docs/04-api.md §4 alpha)

use std::time::Duration;

use chrono::{DateTime, Utc};
use domain::{BookingId, RejectReason};
use reqwest::{Response, StatusCode, header};
use serde::{Deserialize, Serialize};

use crate::{
    AdapterReply, BuildError, Capabilities, CreateBooking, ProviderAdapter, ProviderBooking,
    ProviderError, ProviderPolicy, ResponseMeta,
};

/// 에러 본문을 기록할 때 이 길이까지만 남긴다
const MAX_ERROR_BODY: usize = 500;
/// alpha가 확정된 예약에 쓰는 상태 값
const CONFIRMED: &str = "confirmed";

pub struct AlphaAdapter {
    base_url: String,
    http: reqwest::Client,
}

impl AlphaAdapter {
    /// `base_url` 예: `http://127.0.0.1:9090/alpha/v1`
    pub fn new(base_url: &str, policy: &ProviderPolicy) -> Result<Self, BuildError> {
        // reqwest::Client는 연결을 재사용하므로 하나를 만들어 계속 쓴다 (≈ HttpClient를 싱글턴으로 쓰는 것).
        // 시도 타임아웃은 ResilientProvider가 걸지만, 어댑터를 따로 쓸 때를 위해 같은 값을 안전장치로 둔다.
        let http = reqwest::Client::builder()
            .timeout(policy.attempt_timeout())
            .connect_timeout(Duration::from_secs(1))
            .user_agent(concat!("booking-gateway/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            http,
        })
    }

    async fn create(
        &self,
        request: &CreateBooking,
        meta: &mut ResponseMeta,
    ) -> Result<ProviderBooking, ProviderError> {
        let reference = request.reference.to_string();
        let body = AlphaCreateRequest {
            resource_id: &request.resource_id,
            slot_start: request.slot_start,
            party_size: request.party_size,
            customer: AlphaCustomer {
                name: &request.customer.name,
                phone: &request.customer.phone,
            },
            client_reference: reference.clone(),
        };

        let response = self
            .http
            .post(format!("{}/reservations", self.base_url))
            // 우리 예약 ID를 멱등키로 보낸다. 같은 예약을 다시 보내도 업체가 하나만 만든다.
            .header("Idempotency-Key", &reference)
            .json(&body)
            .send()
            .await
            .map_err(|e| classify_transport(&e))?;
        *meta = read_meta(&response);

        if !response.status().is_success() {
            return Err(classify_failure(response).await);
        }
        let reservation: AlphaReservation =
            response.json().await.map_err(|e| classify_transport(&e))?;
        if reservation.status != CONFIRMED {
            return Err(ProviderError::Decode {
                detail: format!(
                    "생성 응답의 예약 상태가 confirmed가 아닙니다: {}",
                    reservation.status
                ),
            });
        }
        Ok(ProviderBooking {
            provider_booking_id: reservation.id,
        })
    }

    async fn find(
        &self,
        reference: BookingId,
        meta: &mut ResponseMeta,
    ) -> Result<Option<ProviderBooking>, ProviderError> {
        // 참조값은 UUID라 URL에 그대로 넣어도 되는 문자(16진수와 '-')만 있다
        let response = self
            .http
            .get(format!(
                "{}/reservations?client_reference={reference}",
                self.base_url
            ))
            .send()
            .await
            .map_err(|e| classify_transport(&e))?;
        *meta = read_meta(&response);

        if !response.status().is_success() {
            return Err(classify_failure(response).await);
        }
        let list: AlphaReservationList =
            response.json().await.map_err(|e| classify_transport(&e))?;
        // 취소된 예약은 "없음"으로 본다: 다시 만들어도 이중 예약이 아니다
        let mut confirmed = list
            .reservations
            .into_iter()
            .filter(|r| r.status == CONFIRMED);
        let found = confirmed.next();
        if found.is_some() && confirmed.next().is_some() {
            // 이중 예약 의심. 대사 작업(Phase 6)이 이슈로 기록한다. 여기서는 첫 예약을 쓴다.
            tracing::warn!(%reference, "같은 참조값으로 확정된 예약이 업체에 여러 건 있다");
        }
        Ok(found.map(|r| ProviderBooking {
            provider_booking_id: r.id,
        }))
    }
}

// alpha API의 요청 · 응답 형식. 우리 도메인 타입과 따로 두어 업체 API가 바뀌어도 이 파일만 고치면 되게 한다.
#[derive(Serialize)]
struct AlphaCreateRequest<'a> {
    resource_id: &'a str,
    slot_start: DateTime<Utc>,
    party_size: u32,
    customer: AlphaCustomer<'a>,
    client_reference: String,
}

#[derive(Serialize)]
struct AlphaCustomer<'a> {
    name: &'a str,
    phone: &'a str,
}

#[derive(Deserialize)]
struct AlphaReservation {
    id: String,
    status: String,
}

#[derive(Deserialize)]
struct AlphaReservationList {
    reservations: Vec<AlphaReservation>,
}

#[derive(Deserialize)]
struct AlphaErrorBody {
    error: AlphaErrorDetail,
}

#[derive(Deserialize)]
struct AlphaErrorDetail {
    code: String,
}

#[async_trait::async_trait]
impl ProviderAdapter for AlphaAdapter {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            idempotency_key: true,
            lookup_by_reference: true,
            list_bookings: true,
        }
    }

    // 실제 일은 `?`를 쓸 수 있는 내부 함수가 하고, 여기서는 응답 부가 정보(meta)를 결과와 함께 묶어 돌려준다.
    // `&mut meta`: 내부 함수가 응답을 받으면 이 변수를 채운다 (≈ C#의 `ref` 매개변수).
    async fn create_booking(&self, request: &CreateBooking) -> AdapterReply<ProviderBooking> {
        let mut meta = ResponseMeta::default();
        let result = self.create(request, &mut meta).await;
        AdapterReply { result, meta }
    }

    async fn find_booking(&self, reference: BookingId) -> AdapterReply<Option<ProviderBooking>> {
        let mut meta = ResponseMeta::default();
        let result = self.find(reference, &mut meta).await;
        AdapterReply { result, meta }
    }
}

/// 응답 상태 코드와 `X-RateLimit-Remaining` 헤더를 읽는다
fn read_meta(response: &Response) -> ResponseMeta {
    ResponseMeta {
        http_status: Some(response.status().as_u16()),
        rate_limit_remaining: response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse().ok()),
    }
}

/// 성공이 아닌 응답을 분류한다. 4xx는 본문의 에러 코드로 업무적 거절인지 판단한다.
async fn classify_failure(response: Response) -> ProviderError {
    let status = response.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ProviderError::RateLimited {
            retry_after: retry_after(response.headers()),
        };
    }
    if status.is_server_error() {
        return ProviderError::Server {
            status: status.as_u16(),
        };
    }

    let text = response.text().await.unwrap_or_default();
    let code = serde_json::from_str::<AlphaErrorBody>(&text)
        .map(|b| b.error.code)
        .unwrap_or_default();
    match (status, code.as_str()) {
        (StatusCode::CONFLICT, "slot_unavailable") => ProviderError::Rejected {
            reason: RejectReason::SlotUnavailable,
        },
        (StatusCode::NOT_FOUND, "resource_not_found") => ProviderError::Rejected {
            reason: RejectReason::ResourceNotFound,
        },
        (StatusCode::BAD_REQUEST, code) if !code.is_empty() => ProviderError::Rejected {
            reason: RejectReason::InvalidRequest {
                code: code.to_owned(),
            },
        },
        _ => ProviderError::Client {
            status: status.as_u16(),
            body: text.chars().take(MAX_ERROR_BODY).collect(),
        },
    }
}

/// reqwest 에러를 분류한다. 연결 단계 실패인지 먼저 본다 (연결 타임아웃도 "요청이 안 갔음"이므로 Connect).
fn classify_transport(error: &reqwest::Error) -> ProviderError {
    if error.is_connect() {
        ProviderError::Connect {
            detail: error.to_string(),
        }
    } else if error.is_timeout() {
        ProviderError::Timeout
    } else if error.is_decode() {
        ProviderError::Decode {
            detail: error.to_string(),
        }
    } else {
        ProviderError::Transport {
            detail: error.to_string(),
        }
    }
}

/// `Retry-After: <초>` 헤더를 읽는다 (HTTP 날짜 형식은 alpha가 쓰지 않으므로 무시)
fn retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    headers
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}
