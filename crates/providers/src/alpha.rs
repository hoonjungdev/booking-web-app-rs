//! alpha 업체 어댑터 (docs/04-api.md §4 alpha)
//!
//! 모범적인 REST API: 시각은 RFC 3339 UTC, 생성에 `Idempotency-Key`를 지원, 오류는 HTTP 상태 코드 + `{ "error": { "code" } }`,
//! 호출 한도 초과는 429 + `Retry-After`, 응답마다 `X-RateLimit-Remaining`.

use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use domain::{BookingId, RejectReason};
use reqwest::{Response, StatusCode, Url, header};
use serde::{Deserialize, Serialize};

use crate::{
    AdapterReply, AvailableSlot, BuildError, Capabilities, CreateBooking, ListedBooking,
    ListedStatus, ProviderAdapter, ProviderBooking, ProviderError, ResponseMeta, join_url,
    parse_base_url, trace,
};

/// 에러 본문을 기록할 때 이 길이까지만 남긴다
const MAX_ERROR_BODY: usize = 500;
/// alpha가 확정된 예약에 쓰는 상태 값
const CONFIRMED: &str = "confirmed";
/// alpha가 취소된 예약에 쓰는 상태 값
const CANCELLED: &str = "cancelled";

pub struct AlphaAdapter {
    base: Url,
    http: reqwest::Client,
}

impl AlphaAdapter {
    /// `base_url` 예: `http://127.0.0.1:9090/alpha/v1`
    ///
    /// `timeout`은 HTTP 클라이언트의 안전장치다. 실제 시도 타임아웃은 `ResilientProvider`가 정책 값으로 건다
    /// (정책은 실행 중에 바뀔 수 있어서 여기에 박아 두지 않는다. `crate::ADAPTER_TIMEOUT`).
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, BuildError> {
        // reqwest::Client는 연결을 재사용하므로 하나를 만들어 계속 쓴다 (≈ HttpClient를 싱글턴으로 쓰는 것).
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(1))
            .user_agent(concat!("booking-gateway/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base: parse_base_url(base_url)?,
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

        let request = self
            .http
            .post(join_url(&self.base, &["reservations"]))
            // 우리 예약 ID를 멱등키로 보낸다. 같은 예약을 다시 보내도 업체가 하나만 만든다.
            .header("Idempotency-Key", &reference)
            .json(&body);
        // `trace::send`: 그냥 `.send()`와 같지만, 요청에 trace 정보를 실어 업체 쪽 처리까지 한 trace로 이어지게 한다
        let response = trace::send(request, "/reservations")
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
        let mut url = join_url(&self.base, &["reservations"]);
        // `query_pairs_mut`: 쿼리 문자열을 인코딩해 붙인다 (≈ C#의 QueryHelpers.AddQueryString)
        url.query_pairs_mut()
            .append_pair("client_reference", &reference.to_string());
        let response = trace::send(self.http.get(url), "/reservations")
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
            // 이중 예약 의심. 대사 작업이 이슈로 기록한다. 여기서는 첫 예약을 쓴다.
            tracing::warn!(%reference, "같은 참조값으로 확정된 예약이 업체에 여러 건 있다");
        }
        Ok(found.map(|r| ProviderBooking {
            provider_booking_id: r.id,
        }))
    }

    async fn cancel(
        &self,
        provider_booking_id: &str,
        meta: &mut ResponseMeta,
    ) -> Result<(), ProviderError> {
        // `DELETE /reservations/{id}`는 멱등이다: 이미 취소된 예약도 200으로 같은 결과를 돌려준다
        let url = join_url(&self.base, &["reservations", provider_booking_id]);
        let response = trace::send(self.http.delete(url), "/reservations/{id}")
            .await
            .map_err(|e| classify_transport(&e))?;
        *meta = read_meta(&response);

        if !response.status().is_success() {
            return Err(classify_failure(response).await);
        }
        let reservation: AlphaReservation =
            response.json().await.map_err(|e| classify_transport(&e))?;
        if reservation.status != CANCELLED {
            return Err(ProviderError::Decode {
                detail: format!(
                    "취소 응답의 예약 상태가 cancelled가 아닙니다: {}",
                    reservation.status
                ),
            });
        }
        Ok(())
    }

    async fn list(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        meta: &mut ResponseMeta,
    ) -> Result<Vec<ListedBooking>, ProviderError> {
        let mut url = join_url(&self.base, &["reservations"]);
        url.query_pairs_mut()
            .append_pair("from", &from.to_rfc3339())
            .append_pair("to", &to.to_rfc3339());
        let response = trace::send(self.http.get(url), "/reservations")
            .await
            .map_err(|e| classify_transport(&e))?;
        *meta = read_meta(&response);

        if !response.status().is_success() {
            return Err(classify_failure(response).await);
        }
        let list: AlphaReservationList =
            response.json().await.map_err(|e| classify_transport(&e))?;
        list.reservations
            .into_iter()
            .map(|r| {
                let status = match r.status.as_str() {
                    CONFIRMED => ListedStatus::Confirmed,
                    CANCELLED => ListedStatus::Cancelled,
                    other => {
                        return Err(ProviderError::Decode {
                            detail: format!("알 수 없는 예약 상태입니다: {other}"),
                        });
                    }
                };
                Ok(ListedBooking {
                    provider_booking_id: r.id,
                    reference: r.client_reference,
                    slot_start: r.slot_start,
                    status,
                })
            })
            // 하나라도 상태를 읽지 못하면 그 에러를 돌려준다 (≈ 하나라도 실패하면 전체 실패)
            .collect()
    }

    async fn slots(
        &self,
        resource_id: &str,
        date: NaiveDate,
        meta: &mut ResponseMeta,
    ) -> Result<Vec<AvailableSlot>, ProviderError> {
        let mut url = join_url(&self.base, &["availability"]);
        url.query_pairs_mut()
            .append_pair("resource_id", resource_id)
            .append_pair("date", &date.format("%Y-%m-%d").to_string());
        let response = trace::send(self.http.get(url), "/availability")
            .await
            .map_err(|e| classify_transport(&e))?;
        *meta = read_meta(&response);

        if !response.status().is_success() {
            return Err(classify_failure(response).await);
        }
        let availability: AlphaAvailability =
            response.json().await.map_err(|e| classify_transport(&e))?;
        Ok(availability
            .slots
            .into_iter()
            .map(|slot| AvailableSlot {
                start: slot.start,
                end: slot.end,
                remaining: slot.remaining,
            })
            .collect())
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

/// 예약 응답에서 쓰는 필드만 읽는다 (고객 정보는 읽지 않는다)
#[derive(Deserialize)]
struct AlphaReservation {
    id: String,
    status: String,
    slot_start: DateTime<Utc>,
    #[serde(default)]
    client_reference: Option<String>,
}

#[derive(Deserialize)]
struct AlphaReservationList {
    reservations: Vec<AlphaReservation>,
}

#[derive(Deserialize)]
struct AlphaAvailability {
    slots: Vec<AlphaSlot>,
}

#[derive(Deserialize)]
struct AlphaSlot {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    remaining: u32,
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

    async fn cancel_booking(&self, provider_booking_id: &str) -> AdapterReply<()> {
        let mut meta = ResponseMeta::default();
        let result = self.cancel(provider_booking_id, &mut meta).await;
        AdapterReply { result, meta }
    }

    async fn list_bookings(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> AdapterReply<Vec<ListedBooking>> {
        let mut meta = ResponseMeta::default();
        let result = self.list(from, to, &mut meta).await;
        AdapterReply { result, meta }
    }

    async fn availability(
        &self,
        resource_id: &str,
        date: NaiveDate,
    ) -> AdapterReply<Vec<AvailableSlot>> {
        let mut meta = ResponseMeta::default();
        let result = self.slots(resource_id, date, &mut meta).await;
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
        (StatusCode::NOT_FOUND, "reservation_not_found") => ProviderError::Rejected {
            reason: RejectReason::BookingNotFound,
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
pub(crate) fn classify_transport(error: &reqwest::Error) -> ProviderError {
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

/// `Retry-After: <초>` 헤더를 읽는다 (HTTP 날짜 형식은 우리 mock 업체가 쓰지 않으므로 무시)
pub(crate) fn retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    headers
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}
