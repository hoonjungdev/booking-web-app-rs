//! 끝에서 끝까지(E2E): 예약 API → DB → 워커 → alpha 어댑터 → 실제 mock 업체 서버
//!
//! 로드맵 Phase 2 완료 기준 (docs/07-roadmap.md):
//! - 예약을 요청하면 몇 초 뒤 `confirmed`가 된다
//! - 같은 Idempotency-Key로 두 번 보내도 업체 예약은 1건이다

mod common;

use std::{sync::Arc, time::Duration};

use axum::http::{Method, StatusCode};
use booking_server::{
    registry::ProviderRegistry,
    worker::{self, WorkerConfig, WorkerContext},
};
use db::PgPool;
use domain::BookingStatus;
use mock_provider::{
    alpha::model::ReservationQuery,
    chaos::{ChaosConfig, Preset},
};
use tokio_util::sync::CancellationToken;

use common::{booking_body, create_client, send, wait_for_status};

/// mock 업체를 임의 포트에 띄우고, DB의 alpha 주소를 그쪽으로 바꾼다
async fn start_mock(pool: &PgPool, chaos: &ChaosConfig) -> mock_provider::AppState {
    let state = mock_provider::AppState::new(chaos);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("임의 포트에 바인드할 수 있어야 한다");
    let addr = listener.local_addr().expect("주소를 알 수 있어야 한다");
    let app = mock_provider::app(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });

    sqlx::query("UPDATE providers SET base_url = $1, policy = $2 WHERE id = 'alpha'")
        .bind(format!("http://{addr}/alpha/v1"))
        // 테스트가 오래 걸리지 않도록 호출 타임아웃을 짧게 둔다
        .bind(serde_json::json!({ "attempt_timeout_ms": 500 }))
        .execute(pool)
        .await
        .expect("업체 설정을 바꿀 수 있어야 한다");
    state
}

/// DB의 업체 설정으로 실제 어댑터를 만든 워커
async fn real_worker(pool: &PgPool) -> WorkerContext {
    WorkerContext {
        pool: pool.clone(),
        providers: Arc::new(
            ProviderRegistry::load(pool)
                .await
                .expect("어댑터를 만들 수 있어야 한다"),
        ),
        config: WorkerConfig {
            concurrency: 2,
            poll_interval: Duration::from_millis(20),
            lease: Duration::from_secs(30),
        },
    }
}

fn reservations_at(
    mock: &mock_provider::AppState,
) -> Vec<mock_provider::alpha::model::Reservation> {
    mock.alpha
        .lock()
        .expect("잠금이 오염되지 않는다")
        .search(&ReservationQuery::default())
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 접수한_예약이_업체에서_확정되고_재요청해도_업체_예약은_하나다(
    pool: PgPool,
) {
    let mock = start_mock(&pool, &ChaosConfig::passthrough()).await;
    let ctx = real_worker(&pool).await;
    let shutdown = CancellationToken::new();
    // 실제 서버처럼 워커 루프를 백그라운드에서 돌린다
    let worker = tokio::spawn(worker::run(ctx, shutdown.clone()));

    let app = common::app(&pool);
    let client = create_client(&pool, "app").await;
    let headers = [
        ("x-api-key", client.api_key.as_str()),
        ("idempotency-key", "order-1"),
    ];
    let body = booking_body(&common::open_slot());

    let first = send(&app, Method::POST, "/v1/bookings", &headers, Some(&body)).await;
    let second = send(&app, Method::POST, "/v1/bookings", &headers, Some(&body)).await;
    assert_eq!(first.status, StatusCode::ACCEPTED);
    assert_eq!(second.status, StatusCode::ACCEPTED);
    let id = first.booking().id;
    assert_eq!(second.booking().id, id);

    let booking = wait_for_status(
        &app,
        &client.api_key,
        &id.to_string(),
        BookingStatus::Confirmed,
        Duration::from_secs(5),
    )
    .await;

    let reservations = reservations_at(&mock);
    assert_eq!(reservations.len(), 1, "업체에는 예약이 하나만 있어야 한다");
    assert_eq!(
        booking.provider_booking_id.as_deref(),
        Some(reservations[0].id.as_str())
    );
    assert_eq!(
        reservations[0].client_reference.as_deref(),
        Some(id.to_string().as_str()),
        "업체 예약의 참조값은 우리 예약 ID다"
    );

    // 종료 신호를 보내면 워커가 멈춘다
    shutdown.cancel();
    worker.await.expect("워커가 정상 종료되어야 한다");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 응답이_유실되어도_재시도가_업체_예약을_늘리지_않는다(pool: PgPool) {
    // 업체가 예약을 만들고 응답을 보내지 않는 상황 (lossy, 확률 100%)
    let mut lossy = ChaosConfig::passthrough();
    lossy.drop_after_commit_probability = 1.0;
    let mock = start_mock(&pool, &lossy).await;
    let ctx = real_worker(&pool).await;

    let app = common::app(&pool);
    let client = create_client(&pool, "app").await;
    let created = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &[
            ("x-api-key", &client.api_key),
            ("idempotency-key", "order-1"),
        ],
        Some(&booking_body(&common::open_slot())),
    )
    .await
    .booking();

    // 1차 시도: 타임아웃. 하지만 업체에는 이미 예약이 생겼다.
    worker::process_next(&ctx).await.unwrap();
    assert_eq!(reservations_at(&mock).len(), 1);
    let booking = db::bookings::get(&pool, created.id).await.unwrap().unwrap();
    assert_eq!(
        booking.status,
        BookingStatus::Pending,
        "타임아웃은 재시도 대상"
    );

    // 업체가 회복되고, 재시도 시각이 되었다고 치자
    mock.chaos
        .set(mock_provider::Provider::Alpha, Preset::Healthy.config());
    sqlx::query("UPDATE jobs SET run_at = now()")
        .execute(&pool)
        .await
        .unwrap();

    // 2차 시도: 같은 멱등키(우리 예약 ID)로 보내므로 업체는 새로 만들지 않고 기존 예약을 돌려준다
    worker::process_next(&ctx).await.unwrap();
    let booking = db::bookings::get(&pool, created.id).await.unwrap().unwrap();
    assert_eq!(booking.status, BookingStatus::Confirmed);
    assert_eq!(reservations_at(&mock).len(), 1, "이중 예약이 없어야 한다");
}
