//! beta 업체 어댑터 (docs/04-api.md §4 beta): 까다로운 레거시 API
//!
//! alpha와 다른 점을 우리 도메인 모델로 옮기는 것이 이 파일의 일이다.
//!
//! | beta | 우리 쪽 |
//! |---|---|
//! | 필드명 camelCase (`shopCode`, `externalRef`) | `resource_id`, 참조값(예약 ID) |
//! | 시각은 한국 시간 문자열 `yyyyMMddHHmm` (오프셋 없음) | `DateTime<Utc>` |
//! | 업무 오류도 **HTTP 200** `{ "ok": false, "code": "FULL" }` | `ProviderError::Rejected` 등으로 분류 |
//! | 호출 한도 초과는 **503 + Retry-After** (429가 아님) | `RateLimited`. Retry-After 없는 503은 서버 오류 |
//! | 멱등키 없음 | `Capabilities::idempotency_key = false` → 결과를 모르는 실패 뒤에 다시 보내지 않는다 |
//! | rate limit 헤더 없음 | `ResponseMeta::rate_limit_remaining = None` |

use std::time::Duration;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, Timelike, Utc};
use domain::{BookingId, RejectReason};
use reqwest::{Response, StatusCode, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    AdapterReply, AvailableSlot, BuildError, Capabilities, CreateBooking, ListedBooking,
    ListedStatus, ProviderAdapter, ProviderBooking, ProviderError, ResponseMeta,
    alpha::{classify_transport, retry_after},
    join_url, parse_base_url, trace,
};

/// 에러 본문을 기록할 때 이 길이까지만 남긴다
const MAX_ERROR_BODY: usize = 500;
/// beta의 시각 형식: 한국 시간 `yyyyMMddHHmm` (예: `202611201900`)
const TIME_FORMAT: &str = "%Y%m%d%H%M";
/// beta의 날짜 형식: `yyyyMMdd`
const DAY_FORMAT: &str = "%Y%m%d";
const BOOKED: &str = "BOOKED";
const CANCELLED: &str = "CANCELLED";

pub struct BetaAdapter {
    base: Url,
    http: reqwest::Client,
}

impl BetaAdapter {
    /// `base_url` 예: `http://127.0.0.1:9090/beta/api`. `timeout`은 HTTP 클라이언트의 안전장치다 ([`crate::ADAPTER_TIMEOUT`]).
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, BuildError> {
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
        // beta는 분 단위 시각만 표현한다. 19:00:30 같은 시각을 그대로 바꾸면 19:00으로 잘려 다른 슬롯이 예약되므로
        // 보내지 않고 거절한다 (조용히 반올림하지 않는다).
        let Some(start_at) = to_beta_time(request.slot_start) else {
            return Err(ProviderError::Rejected {
                reason: RejectReason::InvalidRequest {
                    code: "slot_not_on_minute".into(),
                },
            });
        };
        let body = BetaBookRequest {
            shop_code: &request.resource_id,
            start_at,
            people: request.party_size,
            guest_name: &request.customer.name,
            guest_phone: &request.customer.phone,
            // 멱등키는 없지만 참조값은 받는다. 결과를 모를 때 이 값으로 조회해 확인한다.
            external_ref: request.reference.to_string(),
        };
        let request = self.http.post(join_url(&self.base, &["book"])).json(&body);
        let booked: BetaBooked = self.send(request, "/book", meta).await?.map_err(|error| {
            match error.code.as_str() {
                "FULL" => rejected(RejectReason::SlotUnavailable),
                "NO_SHOP" => rejected(RejectReason::ResourceNotFound),
                "BAD_TIME" | "PAST" | "BAD_REQUEST" => rejected(RejectReason::InvalidRequest {
                    code: error.code.to_lowercase(),
                }),
                _ => unexpected(&error),
            }
        })?;
        if booked.status != BOOKED {
            return Err(ProviderError::Decode {
                detail: format!(
                    "생성 응답의 예약 상태가 BOOKED가 아닙니다: {}",
                    booked.status
                ),
            });
        }
        Ok(ProviderBooking {
            provider_booking_id: booked.booking_no,
        })
    }

    async fn find(
        &self,
        reference: BookingId,
        meta: &mut ResponseMeta,
    ) -> Result<Option<ProviderBooking>, ProviderError> {
        let url = join_url(&self.base, &["book", "by-ref", &reference.to_string()]);
        let found: Option<BetaBookingList> = self
            .send(self.http.get(url), "/book/by-ref/{ref}", meta)
            .await?
            .map(Some)
            .or_else(|error| match error.code.as_str() {
                // "없음"도 HTTP 200 + ok:false로 온다. 오류가 아니라 정상적인 "없음"이다.
                "NOT_FOUND" => Ok(None),
                _ => Err(unexpected(&error)),
            })?;
        let Some(list) = found else {
            return Ok(None);
        };
        // 취소된 예약은 "없음"으로 본다: 다시 만들어도 이중 예약이 아니다
        let mut booked = list.bookings.into_iter().filter(|b| b.status == BOOKED);
        let first = booked.next();
        if first.is_some() && booked.next().is_some() {
            // 멱등키가 없는 업체라 같은 참조값의 예약이 여러 건일 수 있다. 대사 작업이 이슈로 기록한다.
            tracing::warn!(%reference, "같은 참조값으로 확정된 예약이 업체에 여러 건 있다");
        }
        Ok(first.map(|b| ProviderBooking {
            provider_booking_id: b.booking_no,
        }))
    }

    async fn cancel(&self, booking_no: &str, meta: &mut ResponseMeta) -> Result<(), ProviderError> {
        let url = join_url(&self.base, &["book", booking_no, "cancel"]);
        let cancelled: Option<BetaBooked> = self
            .send(self.http.post(url), "/book/{bookingNo}/cancel", meta)
            .await?
            .map(Some)
            .or_else(|error| match error.code.as_str() {
                // 이미 취소된 예약: 원하던 결과가 이미 이루어졌다 (두 번 보내도 결과가 같다)
                "ALREADY_CANCELLED" => Ok(None),
                "NOT_FOUND" => Err(rejected(RejectReason::BookingNotFound)),
                _ => Err(unexpected(&error)),
            })?;
        if let Some(cancelled) = cancelled
            && cancelled.status != CANCELLED
        {
            return Err(ProviderError::Decode {
                detail: format!(
                    "취소 응답의 예약 상태가 CANCELLED가 아닙니다: {}",
                    cancelled.status
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
        // beta는 한국 날짜 단위로만 묻는다. [from, to)를 덮는 날짜 범위로 물은 뒤 시각으로 다시 거른다.
        let from_day = from.with_timezone(&kst()).date_naive();
        let last_instant = to - chrono::TimeDelta::microseconds(1);
        let to_day = last_instant
            .with_timezone(&kst())
            .date_naive()
            .max(from_day);
        let mut url = join_url(&self.base, &["bookings"]);
        url.query_pairs_mut()
            .append_pair("fromDay", &from_day.format(DAY_FORMAT).to_string())
            .append_pair("toDay", &to_day.format(DAY_FORMAT).to_string());
        let list: BetaBookingList = self
            .send(self.http.get(url), "/bookings", meta)
            .await?
            .map_err(|error| unexpected(&error))?;

        let mut listed = Vec::with_capacity(list.bookings.len());
        for booking in list.bookings {
            let slot_start =
                from_beta_time(&booking.start_at).ok_or_else(|| ProviderError::Decode {
                    detail: format!("startAt 형식이 올바르지 않습니다: {}", booking.start_at),
                })?;
            if slot_start < from || slot_start >= to {
                continue;
            }
            let status = match booking.status.as_str() {
                BOOKED => ListedStatus::Confirmed,
                CANCELLED => ListedStatus::Cancelled,
                other => {
                    return Err(ProviderError::Decode {
                        detail: format!("알 수 없는 예약 상태입니다: {other}"),
                    });
                }
            };
            listed.push(ListedBooking {
                provider_booking_id: booking.booking_no,
                reference: booking.external_ref,
                slot_start,
                status,
            });
        }
        Ok(listed)
    }

    async fn slots(
        &self,
        shop_code: &str,
        day: NaiveDate,
        meta: &mut ResponseMeta,
    ) -> Result<Vec<AvailableSlot>, ProviderError> {
        let mut url = join_url(&self.base, &["slots"]);
        url.query_pairs_mut()
            .append_pair("shopCode", shop_code)
            .append_pair("day", &day.format(DAY_FORMAT).to_string());
        let slots: BetaSlots = self
            .send(self.http.get(url), "/slots", meta)
            .await?
            .map_err(|error| match error.code.as_str() {
                "NO_SHOP" => rejected(RejectReason::ResourceNotFound),
                "BAD_REQUEST" => rejected(RejectReason::InvalidRequest {
                    code: "bad_request".into(),
                }),
                _ => unexpected(&error),
            })?;
        slots
            .slots
            .into_iter()
            .map(|slot| {
                let start = from_beta_time(&slot.start_at);
                let end = from_beta_time(&slot.end_at);
                match (start, end) {
                    (Some(start), Some(end)) => Ok(AvailableSlot {
                        start,
                        end,
                        remaining: slot.remain,
                    }),
                    _ => Err(ProviderError::Decode {
                        detail: format!(
                            "슬롯 시각 형식이 올바르지 않습니다: {} ~ {}",
                            slot.start_at, slot.end_at
                        ),
                    }),
                }
            })
            .collect()
    }

    /// 요청을 보내고 beta의 응답 봉투(`ok` + 본문 또는 오류 코드)를 푼다.
    ///
    /// - 바깥 `Result`: 기술적 실패 (연결 · 타임아웃 · 5xx · 503 호출 제한 · 형식 오류). `?`로 바로 돌려준다.
    /// - 안쪽 `Result`: HTTP 200 안의 결과. `ok: true`면 본문(`T`), `ok: false`면 오류 코드(`String`).
    ///   오류 코드를 어떻게 볼지는 부르는 쪽이 정한다 (같은 `NOT_FOUND`가 조회에서는 정상, 취소에서는 거절).
    async fn send<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        route: &'static str,
        meta: &mut ResponseMeta,
    ) -> Result<Result<T, ErrorCode>, ProviderError> {
        let response = trace::send(request, route)
            .await
            .map_err(|e| classify_transport(&e))?;
        // beta는 rate limit 헤더를 보내지 않는다. 상태 코드만 남긴다.
        meta.http_status = Some(response.status().as_u16());
        if !response.status().is_success() {
            return Err(classify_http_failure(response).await);
        }
        let text = response.text().await.map_err(|e| classify_transport(&e))?;
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| ProviderError::Decode {
                detail: format!("{error}: {}", truncate(&text)),
            })?;
        match value.get("ok").and_then(serde_json::Value::as_bool) {
            Some(true) => {
                let body =
                    serde_json::from_value(value).map_err(|error| ProviderError::Decode {
                        detail: format!("{error}: {}", truncate(&text)),
                    })?;
                Ok(Ok(body))
            }
            Some(false) => {
                let code = value
                    .get("code")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Ok(Err(ErrorCode { code, text }))
            }
            None => Err(ProviderError::Decode {
                detail: format!("응답에 ok 필드가 없습니다: {}", truncate(&text)),
            }),
        }
    }
}

/// beta가 HTTP 200으로 돌려준 오류 (`{ "ok": false, "code": … }`)
struct ErrorCode {
    code: String,
    /// 응답 원문 (호출 기록의 `error_detail`에만 남긴다)
    text: String,
}

fn rejected(reason: RejectReason) -> ProviderError {
    ProviderError::Rejected { reason }
}

/// 우리가 모르는 오류 코드. `ok: false`라 업체가 처리하지 않은 것은 확실하므로 `Client`로 둔다
/// (다시 보내지 않고, 결과 불명으로 보지도 않는다). 원문은 호출 기록에만 남는다 (`ProviderError::detail`).
fn unexpected(error: &ErrorCode) -> ProviderError {
    ProviderError::Client {
        status: 200,
        body: truncate(&error.text),
    }
}

// beta API의 요청 · 응답 형식 (camelCase)
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BetaBookRequest<'a> {
    shop_code: &'a str,
    start_at: String,
    people: u32,
    guest_name: &'a str,
    guest_phone: &'a str,
    external_ref: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BetaBooked {
    booking_no: String,
    status: String,
}

/// 예약 목록에서 쓰는 필드만 읽는다 (손님 이름 · 전화번호는 읽지 않는다)
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BetaBooking {
    booking_no: String,
    start_at: String,
    status: String,
    #[serde(default)]
    external_ref: Option<String>,
}

#[derive(Deserialize)]
struct BetaBookingList {
    bookings: Vec<BetaBooking>,
}

#[derive(Deserialize)]
struct BetaSlots {
    slots: Vec<BetaSlot>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BetaSlot {
    start_at: String,
    end_at: String,
    remain: u32,
}

#[async_trait::async_trait]
impl ProviderAdapter for BetaAdapter {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // 같은 요청을 두 번 보내면 예약이 두 건 생긴다
            idempotency_key: false,
            lookup_by_reference: true,
            list_bookings: true,
        }
    }

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

/// 200이 아닌 응답을 분류한다. beta는 업무 오류를 200으로 보내므로, 200이 아니면 기술적 문제다.
///
/// - 503 + `Retry-After`: 호출 한도 초과 (beta는 429 대신 이렇게 알린다)
/// - `Retry-After` 없는 503, 그 밖의 5xx: 서버 오류
/// - 429: beta가 쓰지 않지만 오면 한도 초과로 본다
/// - 그 밖의 4xx: 예상하지 못한 응답 (대개 우리 쪽 버그: 없는 경로 등)
async fn classify_http_failure(response: Response) -> ProviderError {
    let status = response.status();
    let wait = retry_after(response.headers());
    if status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::SERVICE_UNAVAILABLE && wait.is_some())
    {
        return ProviderError::RateLimited { retry_after: wait };
    }
    if status.is_server_error() {
        return ProviderError::Server {
            status: status.as_u16(),
        };
    }
    let text = response.text().await.unwrap_or_default();
    ProviderError::Client {
        status: status.as_u16(),
        body: truncate(&text),
    }
}

fn truncate(text: &str) -> String {
    text.chars().take(MAX_ERROR_BODY).collect()
}

/// 한국 표준시 (UTC+9). beta는 오프셋 없이 한국 시간을 쓴다.
fn kst() -> FixedOffset {
    FixedOffset::east_opt(9 * 3600).expect("UTC+9는 항상 올바른 오프셋이다")
}

/// UTC 시각 → beta 시각 문자열 (한국 시간 `yyyyMMddHHmm`).
/// 초나 그보다 작은 단위가 있으면 `None`: beta는 분 단위로만 표현해서 바꾸면 다른 시각이 된다.
pub(crate) fn to_beta_time(time: DateTime<Utc>) -> Option<String> {
    if time.second() != 0 || time.nanosecond() != 0 {
        return None;
    }
    Some(time.with_timezone(&kst()).format(TIME_FORMAT).to_string())
}

/// beta 시각 문자열(한국 시간 `yyyyMMddHHmm`) → UTC. 형식이 틀리면 `None`.
pub(crate) fn from_beta_time(text: &str) -> Option<DateTime<Utc>> {
    // chrono의 형식 해석은 자릿수에 너그럽다 (`%M`이 "0" 한 자리도 받는다). 정확히 숫자 12자리인지 먼저 본다.
    if text.len() != 12 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // NaiveDateTime: 시간대가 붙지 않은 "벽시계 시각" (≈ C#의 DateTimeKind.Unspecified인 DateTime)
    let local = NaiveDateTime::parse_from_str(text, TIME_FORMAT).ok()?;
    // 한국 시간으로 해석한다. UTC+9는 서머타임이 없어 언제나 정확히 하나로 정해진다 (`single`)
    let time = local.and_local_timezone(kst()).single()?;
    Some(time.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc를_한국_시간_문자열로_바꾼다() {
        let utc: DateTime<Utc> = "2026-11-20T10:00:00Z".parse().unwrap();
        assert_eq!(to_beta_time(utc).as_deref(), Some("202611201900"));
        // 한국 날짜로는 다음 날이 되는 시각
        let late: DateTime<Utc> = "2026-11-20T16:30:00Z".parse().unwrap();
        assert_eq!(to_beta_time(late).as_deref(), Some("202611210130"));
    }

    #[test]
    fn 분_단위가_아닌_시각은_바꾸지_않는다() {
        let with_seconds: DateTime<Utc> = "2026-11-20T10:00:30Z".parse().unwrap();
        assert_eq!(to_beta_time(with_seconds), None);
        let with_micros: DateTime<Utc> = "2026-11-20T10:00:00.000001Z".parse().unwrap();
        assert_eq!(to_beta_time(with_micros), None);
    }

    #[test]
    fn 한국_시간_문자열을_utc로_읽는다() {
        let parsed = from_beta_time("202611201900").unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-11-20T10:00:00+00:00");
        assert_eq!(
            from_beta_time("202611210130").unwrap().to_rfc3339(),
            "2026-11-20T16:30:00+00:00"
        );
        // 왕복
        assert_eq!(to_beta_time(parsed).as_deref(), Some("202611201900"));
        for bad in [
            "",
            "2026-11-20",
            "202611201960",
            "20261120190",
            "2026112019000",
        ] {
            assert_eq!(from_beta_time(bad), None, "{bad}");
        }
    }
}
