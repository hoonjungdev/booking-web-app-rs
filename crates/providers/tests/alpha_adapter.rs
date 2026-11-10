//! alpha 어댑터를 실제 mock 업체 서버에 붙여 검증한다.
//!
//! 테스트마다 mock-provider를 임의 포트(`127.0.0.1:0`)에 띄운다. 업체 응답을 흉내 내는 가짜 대신
//! Phase 1에서 만든 mock 업체를 그대로 쓰므로, 멱등키 · 정원 · 장애 주입이 실제와 같은 방식으로 동작한다.
//! 어댑터는 한 번만 보내므로(재시도는 ResilientProvider 몫) 여기서는 분류가 맞는지만 본다.

use std::time::Duration;

use chrono::{Days, Utc};
use domain::{BookingId, Customer, RejectReason};
use mock_provider::{AppState, catalog, chaos::ChaosConfig};
use providers::{
    AlphaAdapter, CreateBooking, ProviderAdapter, ProviderBooking, ProviderError, ProviderPolicy,
};
use uuid::Uuid;

/// mock 업체를 띄우고 alpha API 주소와 상태(장애 설정 · 저장된 예약 확인용)를 돌려준다
async fn spawn_mock(config: &ChaosConfig) -> (String, AppState) {
    let state = AppState::new(config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("임의 포트에 바인드할 수 있어야 한다");
    let addr = listener
        .local_addr()
        .expect("바인드한 주소를 알 수 있어야 한다");
    let app = mock_provider::app(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}/alpha/v1"), state)
}

fn adapter(base_url: &str, timeout_ms: u64) -> AlphaAdapter {
    AlphaAdapter::new(
        base_url,
        &ProviderPolicy {
            attempt_timeout_ms: timeout_ms,
            ..ProviderPolicy::default()
        },
    )
    .expect("HTTP 클라이언트를 만들 수 있어야 한다")
}

/// 내일 정원이 1 이상인 첫 슬롯
fn open_slot() -> catalog::Slot {
    let tomorrow = Utc::now()
        .with_timezone(&catalog::kst())
        .date_naive()
        .checked_add_days(Days::new(1))
        .expect("내일 날짜는 항상 계산할 수 있다");
    catalog::slots_for("store-001", tomorrow)
        .into_iter()
        .find(|s| s.capacity > 0)
        .expect("내일 열린 슬롯이 하나는 있어야 한다")
}

fn create_request(slot: &catalog::Slot) -> CreateBooking {
    CreateBooking {
        reference: BookingId::from_uuid(Uuid::now_v7()),
        resource_id: "store-001".into(),
        slot_start: slot.start,
        party_size: 2,
        customer: Customer {
            name: "홍길동".into(),
            phone: "010-0000-0000".into(),
        },
    }
}

#[tokio::test]
async fn 예약을_만들고_같은_참조값으로_다시_보내도_하나만_생긴다() {
    let (url, mock) = spawn_mock(&ChaosConfig::passthrough()).await;
    let alpha = adapter(&url, 1000);
    let request = create_request(&open_slot());

    let first = alpha.create_booking(&request).await;
    let second = alpha.create_booking(&request).await;

    assert_eq!(first.meta.http_status, Some(201), "새로 만듦");
    assert_eq!(
        second.meta.http_status,
        Some(200),
        "같은 멱등키 → 이전 결과"
    );
    let first = first.result.unwrap();
    assert_eq!(first, second.result.unwrap());
    assert!(first.provider_booking_id.starts_with("rsv_"));
    let stored = mock_provider::alpha::model::ReservationQuery::default();
    let reservations = mock.alpha.lock().unwrap().search(&stored);
    assert_eq!(reservations.len(), 1);
    assert_eq!(
        reservations[0].client_reference.as_deref(),
        Some(request.reference.to_string().as_str())
    );
}

#[tokio::test]
async fn 응답의_rate_limit_잔여량을_읽는다() {
    let mut config = ChaosConfig::passthrough();
    config.rate_limit.per_second = 10;
    config.rate_limit.burst = 5;
    let (url, _mock) = spawn_mock(&config).await;
    let alpha = adapter(&url, 1000);

    let reply = alpha.create_booking(&create_request(&open_slot())).await;

    assert!(reply.result.is_ok());
    assert_eq!(
        reply.meta.rate_limit_remaining,
        Some(4),
        "burst 5에서 하나 씀"
    );
}

#[tokio::test]
async fn 참조값으로_확정된_예약을_찾는다() {
    let (url, mock) = spawn_mock(&ChaosConfig::passthrough()).await;
    let alpha = adapter(&url, 1000);
    let request = create_request(&open_slot());
    let created = alpha.create_booking(&request).await.result.unwrap();

    let found = alpha.find_booking(request.reference).await;
    assert_eq!(found.meta.http_status, Some(200));
    assert_eq!(found.result, Ok(Some(created.clone())));

    // 없는 참조값
    let missing = alpha
        .find_booking(BookingId::from_uuid(Uuid::now_v7()))
        .await;
    assert_eq!(missing.result, Ok(None));

    // 업체 쪽에서 취소된 예약은 "확정된 예약 없음"으로 본다
    mock.alpha
        .lock()
        .unwrap()
        .cancel(&created.provider_booking_id, Utc::now())
        .unwrap();
    let cancelled = alpha.find_booking(request.reference).await;
    assert_eq!(cancelled.result, Ok(None::<ProviderBooking>));
}

#[tokio::test]
async fn 정원이_차면_업무적_거절로_분류한다() {
    let (url, _mock) = spawn_mock(&ChaosConfig::passthrough()).await;
    let alpha = adapter(&url, 1000);
    let slot = open_slot();
    for _ in 0..slot.capacity {
        alpha
            .create_booking(&create_request(&slot))
            .await
            .result
            .unwrap();
    }

    let reply = alpha.create_booking(&create_request(&slot)).await;
    assert_eq!(reply.meta.http_status, Some(409));
    assert_eq!(
        reply.result,
        Err(ProviderError::Rejected {
            reason: RejectReason::SlotUnavailable
        })
    );
}

#[tokio::test]
async fn 슬롯_경계가_아닌_시각은_invalid_request로_거절된다() {
    let (url, _mock) = spawn_mock(&ChaosConfig::passthrough()).await;
    let alpha = adapter(&url, 1000);
    let mut request = create_request(&open_slot());
    request.slot_start += chrono::TimeDelta::minutes(30);

    let result = alpha.create_booking(&request).await.result;
    assert_eq!(
        result,
        Err(ProviderError::Rejected {
            reason: RejectReason::InvalidRequest {
                code: "invalid_slot".into()
            }
        })
    );
}

#[tokio::test]
async fn 장애_상황을_분류한다() {
    // outage → 503
    let mut outage = ChaosConfig::passthrough();
    outage.outage = true;
    let (url, _mock) = spawn_mock(&outage).await;
    let alpha = adapter(&url, 1000);
    let result = alpha.create_booking(&create_request(&open_slot())).await;
    assert_eq!(result.result, Err(ProviderError::Server { status: 503 }));
    let result = alpha
        .find_booking(BookingId::from_uuid(Uuid::now_v7()))
        .await;
    assert_eq!(result.result, Err(ProviderError::Server { status: 503 }));

    // 호출 한도 초과 → 429 + Retry-After
    let mut throttled = ChaosConfig::passthrough();
    throttled.rate_limit.per_second = 1;
    throttled.rate_limit.burst = 1;
    let (url, _mock) = spawn_mock(&throttled).await;
    let alpha = adapter(&url, 1000);
    alpha
        .create_booking(&create_request(&open_slot()))
        .await
        .result
        .unwrap();
    let reply = alpha.create_booking(&create_request(&open_slot())).await;
    assert_eq!(reply.meta.http_status, Some(429));
    assert_eq!(reply.meta.rate_limit_remaining, Some(0));
    assert_eq!(
        reply.result,
        Err(ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(1))
        })
    );
}

#[tokio::test]
async fn 응답이_없으면_타임아웃으로_분류한다() {
    let mut hang = ChaosConfig::passthrough();
    hang.hang_probability = 1.0;
    let (url, _mock) = spawn_mock(&hang).await;

    let started = std::time::Instant::now();
    let reply = adapter(&url, 200)
        .create_booking(&create_request(&open_slot()))
        .await;

    assert_eq!(reply.result, Err(ProviderError::Timeout));
    assert_eq!(reply.meta.http_status, None, "응답을 받지 못했다");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "타임아웃 설정대로 빨리 끝나야 한다"
    );
}

#[tokio::test]
async fn 연결할_수_없으면_connect로_분류한다() {
    // 포트를 하나 받아 바로 닫으면, 그 포트에는 아무도 듣고 있지 않다
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let reply = adapter(&format!("http://{addr}/alpha/v1"), 1000)
        .create_booking(&create_request(&open_slot()))
        .await;
    let error = reply.result.unwrap_err();
    assert!(matches!(error, ProviderError::Connect { .. }), "{error:?}");
    assert!(error.definitely_not_processed());
}
