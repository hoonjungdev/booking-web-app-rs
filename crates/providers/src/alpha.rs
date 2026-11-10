//! alpha 업체 어댑터 (docs/04-api.md §4 alpha)

use std::time::Duration;

use chrono::{DateTime, Utc};
use domain::RejectReason;
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};

use crate::{
    BuildError, Capabilities, CreateBooking, ProviderAdapter, ProviderBooking, ProviderError,
    ProviderPolicy,
};

/// 에러 본문을 기록할 때 이 길이까지만 남긴다
const MAX_ERROR_BODY: usize = 500;

pub struct AlphaAdapter {
    base_url: String,
    http: reqwest::Client,
}

impl AlphaAdapter {
    /// `base_url` 예: `http://127.0.0.1:9090/alpha/v1`
    pub fn new(base_url: &str, policy: &ProviderPolicy) -> Result<Self, BuildError> {
        // reqwest::Client는 연결을 재사용하므로 하나를 만들어 계속 쓴다 (≈ HttpClient를 싱글턴으로 쓰는 것)
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(policy.attempt_timeout_ms))
            .connect_timeout(Duration::from_secs(1))
            .user_agent(concat!("booking-gateway/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            http,
        })
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

    async fn create_booking(
        &self,
        request: &CreateBooking,
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

        let status = response.status();
        if status.is_success() {
            let reservation: AlphaReservation =
                response.json().await.map_err(|e| classify_transport(&e))?;
            if reservation.status != "confirmed" {
                return Err(ProviderError::Decode {
                    detail: format!(
                        "생성 응답의 예약 상태가 confirmed가 아닙니다: {}",
                        reservation.status
                    ),
                });
            }
            return Ok(ProviderBooking {
                provider_booking_id: reservation.id,
            });
        }

        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::RateLimited {
                retry_after: retry_after(response.headers()),
            });
        }
        if status.is_server_error() {
            return Err(ProviderError::Server {
                status: status.as_u16(),
            });
        }

        // 4xx: 본문의 에러 코드로 업무적 거절인지 판단한다
        let text = response.text().await.unwrap_or_default();
        let code = serde_json::from_str::<AlphaErrorBody>(&text)
            .map(|b| b.error.code)
            .unwrap_or_default();
        Err(match (status, code.as_str()) {
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
        })
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
