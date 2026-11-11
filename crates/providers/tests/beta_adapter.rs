//! beta 어댑터를 실제 mock 업체 서버에 붙여 검증한다 (docs/04-api.md §4 beta).
//!
//! beta는 까다로운 레거시 API다: 한국 시간 문자열(`yyyyMMddHHmm`), HTTP 200으로 오는 업무 오류, 멱등키 없음,
//! 호출 한도 초과는 503 + `Retry-After`. 어댑터가 이 차이를 우리 도메인 타입과 분류로 옮기는지 본다.

use std::time::Duration;

use chrono::{Days, TimeDelta, Utc};
use domain::{BookingId, Customer, RejectReason};
use mock_provider::{AppState, catalog, chaos::ChaosConfig};
use providers::{
    BetaAdapter, CreateBooking, ListedStatus, ProviderAdapter, ProviderBooking, ProviderError,
};
use uuid::Uuid;

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
    (format!("http://{addr}/beta/api"), state)
}

fn adapter(base_url: &str, timeout_ms: u64) -> BetaAdapter {
    BetaAdapter::new(base_url, Duration::from_millis(timeout_ms))
        .expect("HTTP 클라이언트를 만들 수 있어야 한다")
}

/// 내일 정원이 `min` 이상인 첫 슬롯 (강남점 GN01)
fn open_slot(min: u32) -> catalog::Slot {
    let tomorrow = Utc::now()
        .with_timezone(&catalog::kst())
        .date_naive()
        .checked_add_days(Days::new(1))
        .expect("내일 날짜는 항상 계산할 수 있다");
    catalog::slots_for("GN01", tomorrow)
        .into_iter()
        .find(|s| s.capacity >= min)
        .expect("내일 열린 슬롯이 하나는 있어야 한다")
}

fn request(slot: &catalog::Slot) -> CreateBooking {
    CreateBooking {
        reference: BookingId::from_uuid(Uuid::now_v7()),
        resource_id: "GN01".into(),
        slot_start: slot.start,
        party_size: 2,
        customer: Customer {
            name: "홍길동".into(),
            phone: "010-0000-0000".into(),
        },
    }
}

#[tokio::test]
async fn 예약을_만들고_같은_요청을_다시_보내면_두_건이_생긴다() {
    let (url, mock) = spawn_mock(&ChaosConfig::passthrough()).await;
    let beta = adapter(&url, 1000);
    let slot = open_slot(2);
    let request = request(&slot);

    let first = beta.create_booking(&request).await;
    assert_eq!(first.meta.http_status, Some(200));
    assert_eq!(
        first.meta.rate_limit_remaining, None,
        "beta는 rate limit 헤더가 없다"
    );
    let first = first.result.unwrap();
    assert!(first.provider_booking_id.starts_with("BK"));

    // 멱등키가 없다: 같은 요청을 다시 보내면 업체에 예약이 하나 더 생긴다 (그래서 결과를 모르면 다시 보내지 않는다)
    let second = beta.create_booking(&request).await.result.unwrap();
    assert_ne!(first, second);
    let stored = mock
        .beta
        .lock()
        .unwrap()
        .by_ref(&request.reference.to_string());
    assert_eq!(stored.len(), 2);
    assert_eq!(
        stored[0].slot_start, slot.start,
        "한국 시간 문자열로 바꿔 보냈다"
    );
}

#[tokio::test]
async fn http_200으로_온_업무_오류를_분류한다() {
    let (url, _mock) = spawn_mock(&ChaosConfig::passthrough()).await;
    let beta = adapter(&url, 1000);
    let slot = open_slot(1);
    for _ in 0..slot.capacity {
        beta.create_booking(&request(&slot)).await.result.unwrap();
    }

    let full = beta.create_booking(&request(&slot)).await;
    assert_eq!(full.meta.http_status, Some(200), "업무 오류도 200으로 온다");
    assert_eq!(
        full.result,
        Err(ProviderError::Rejected {
            reason: RejectReason::SlotUnavailable
        })
    );

    let mut no_shop = request(&slot);
    no_shop.resource_id = "ZZ99".into();
    assert_eq!(
        beta.create_booking(&no_shop).await.result,
        Err(ProviderError::Rejected {
            reason: RejectReason::ResourceNotFound
        })
    );

    let mut not_a_slot = request(&slot);
    not_a_slot.slot_start += TimeDelta::minutes(30);
    assert_eq!(
        beta.create_booking(&not_a_slot).await.result,
        Err(ProviderError::Rejected {
            reason: RejectReason::InvalidRequest {
                code: "bad_time".into()
            }
        })
    );
}

#[tokio::test]
async fn 분_단위가_아닌_시각은_보내지_않고_거절한다() {
    let (url, mock) = spawn_mock(&ChaosConfig::passthrough()).await;
    let beta = adapter(&url, 1000);
    let mut request = request(&open_slot(1));
    // beta는 분 단위로만 표현한다. 19:00:30을 그대로 바꾸면 19:00 슬롯이 예약된다.
    request.slot_start += TimeDelta::seconds(30);

    let reply = beta.create_booking(&request).await;
    assert_eq!(reply.meta.http_status, None, "업체를 부르지 않았다");
    assert_eq!(
        reply.result,
        Err(ProviderError::Rejected {
            reason: RejectReason::InvalidRequest {
                code: "slot_not_on_minute".into()
            }
        })
    );
    assert!(mock.beta.lock().unwrap().all().is_empty());
}

#[tokio::test]
async fn 참조값으로_확정된_예약만_찾는다() {
    let (url, mock) = spawn_mock(&ChaosConfig::passthrough()).await;
    let beta = adapter(&url, 1000);
    let request = request(&open_slot(1));
    let created = beta.create_booking(&request).await.result.unwrap();

    assert_eq!(
        beta.find_booking(request.reference).await.result,
        Ok(Some(created.clone()))
    );
    // 없는 참조값도 200 + NOT_FOUND로 온다. 오류가 아니라 "없음"이다.
    let missing = beta
        .find_booking(BookingId::from_uuid(Uuid::now_v7()))
        .await;
    assert_eq!(missing.meta.http_status, Some(200));
    assert_eq!(missing.result, Ok(None));

    mock.beta
        .lock()
        .unwrap()
        .cancel(&created.provider_booking_id, Utc::now())
        .unwrap();
    assert_eq!(
        beta.find_booking(request.reference).await.result,
        Ok(None::<ProviderBooking>),
        "취소된 예약은 없는 것으로 본다"
    );
}

#[tokio::test]
async fn 이미_취소된_예약을_다시_취소해도_성공이다() {
    let (url, _mock) = spawn_mock(&ChaosConfig::passthrough()).await;
    let beta = adapter(&url, 1000);
    let created = beta
        .create_booking(&request(&open_slot(1)))
        .await
        .result
        .unwrap();

    assert_eq!(
        beta.cancel_booking(&created.provider_booking_id)
            .await
            .result,
        Ok(())
    );
    // beta는 두 번째 취소에 ALREADY_CANCELLED를 돌려준다. 원하던 결과이므로 성공이다 (다시 보내도 안전한 근거)
    assert_eq!(
        beta.cancel_booking(&created.provider_booking_id)
            .await
            .result,
        Ok(())
    );
    assert_eq!(
        beta.cancel_booking("BK99999999").await.result,
        Err(ProviderError::Rejected {
            reason: RejectReason::BookingNotFound
        })
    );
}

#[tokio::test]
async fn 기간별_목록은_한국_날짜로_묻고_시각으로_다시_거른다() {
    let (url, mock) = spawn_mock(&ChaosConfig::passthrough()).await;
    let beta = adapter(&url, 1000);
    let slot = open_slot(2);
    let kept = request(&slot);
    let kept_booking = beta.create_booking(&kept).await.result.unwrap();
    let other = beta.create_booking(&request(&slot)).await.result.unwrap();
    mock.beta
        .lock()
        .unwrap()
        .cancel(&other.provider_booking_id, Utc::now())
        .unwrap();

    let listed = beta
        .list_bookings(slot.start, slot.end)
        .await
        .result
        .unwrap();
    assert_eq!(listed.len(), 2);
    let row = listed
        .iter()
        .find(|b| b.provider_booking_id == kept_booking.provider_booking_id)
        .unwrap();
    assert_eq!(
        row.reference.as_deref(),
        Some(kept.reference.to_string().as_str())
    );
    assert_eq!(row.slot_start, slot.start);
    assert_eq!(row.status, ListedStatus::Confirmed);
    assert!(
        listed
            .iter()
            .any(|b| b.provider_booking_id == other.provider_booking_id
                && b.status == ListedStatus::Cancelled)
    );

    // beta는 하루 단위로 돌려주지만, 우리가 물은 시각 범위 밖은 빼고 돌려준다
    let before = beta
        .list_bookings(slot.start - TimeDelta::hours(1), slot.start)
        .await
        .result
        .unwrap();
    assert!(before.is_empty());
}

#[tokio::test]
async fn 빈_시간은_한국_시간_문자열을_utc로_바꿔_돌려준다() {
    let (url, _mock) = spawn_mock(&ChaosConfig::passthrough()).await;
    let beta = adapter(&url, 1000);
    let slot = open_slot(1);
    let day = slot.start.with_timezone(&catalog::kst()).date_naive();
    beta.create_booking(&request(&slot)).await.result.unwrap();

    let slots = beta.availability("GN01", day).await.result.unwrap();
    assert_eq!(slots.len(), 11);
    let booked = slots.iter().find(|s| s.start == slot.start).unwrap();
    assert_eq!(booked.end, slot.end);
    assert_eq!(booked.remaining, slot.capacity - 1);

    assert_eq!(
        beta.availability("ZZ99", day).await.result,
        Err(ProviderError::Rejected {
            reason: RejectReason::ResourceNotFound
        })
    );
}

#[tokio::test]
async fn 장애_상황을_분류한다() {
    // 점검(outage): 503인데 Retry-After가 없다 → 서버 오류
    let outage = ChaosConfig {
        outage: true,
        ..ChaosConfig::passthrough()
    };
    let (url, _mock) = spawn_mock(&outage).await;
    let result = adapter(&url, 1000)
        .create_booking(&request(&open_slot(1)))
        .await;
    assert_eq!(result.result, Err(ProviderError::Server { status: 503 }));

    // 호출 한도 초과: 429가 아니라 503 + Retry-After
    let mut throttled = ChaosConfig::passthrough();
    throttled.rate_limit.per_second = 1;
    throttled.rate_limit.burst = 1;
    let (url, _mock) = spawn_mock(&throttled).await;
    let beta = adapter(&url, 1000);
    beta.create_booking(&request(&open_slot(2)))
        .await
        .result
        .unwrap();
    let limited = beta.create_booking(&request(&open_slot(2))).await;
    assert_eq!(limited.meta.http_status, Some(503));
    assert_eq!(
        limited.result,
        Err(ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(1))
        })
    );

    // 주입한 5xx
    let mut failing = ChaosConfig::passthrough();
    failing.error.probability = 1.0;
    failing.error.statuses = vec![502];
    let (url, _mock) = spawn_mock(&failing).await;
    assert_eq!(
        adapter(&url, 1000)
            .create_booking(&request(&open_slot(1)))
            .await
            .result,
        Err(ProviderError::Server { status: 502 })
    );

    // 응답 없음 → 타임아웃
    let hang = ChaosConfig {
        hang_probability: 1.0,
        ..ChaosConfig::passthrough()
    };
    let (url, _mock) = spawn_mock(&hang).await;
    assert_eq!(
        adapter(&url, 200)
            .find_booking(BookingId::from_uuid(Uuid::now_v7()))
            .await
            .result,
        Err(ProviderError::Timeout)
    );
}
