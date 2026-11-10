//! 예약 API 테스트 (docs/04-api.md §2)

mod common;

use axum::http::{Method, StatusCode};
use chrono::{DateTime, FixedOffset, TimeDelta, Utc};
use db::PgPool;
use domain::BookingStatus;
use serde_json::json;

use common::{booking_body, create_client, send};

fn future_slot() -> String {
    (Utc::now() + TimeDelta::days(1)).to_rfc3339()
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn api_키가_없거나_틀리면_401(pool: PgPool) {
    let app = common::app(&pool);
    let body = booking_body(&future_slot());

    let missing = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &[("idempotency-key", "k")],
        Some(&body),
    )
    .await;
    let wrong = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &[("x-api-key", "bk_wrong"), ("idempotency-key", "k")],
        Some(&body),
    )
    .await;

    for reply in [missing, wrong] {
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.headers["content-type"], "application/problem+json");
        assert_eq!(
            reply.body["type"],
            "https://booking.local/problems/unauthorized"
        );
        assert_eq!(reply.body["status"], 401);
    }
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 예약을_접수하면_202와_location을_돌려주고_조회할_수_있다(pool: PgPool) {
    let app = common::app(&pool);
    let client = create_client(&pool, "app").await;
    let slot = future_slot();

    let created = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &[
            ("x-api-key", &client.api_key),
            ("idempotency-key", "order-1"),
        ],
        Some(&booking_body(&slot)),
    )
    .await;

    assert_eq!(created.status, StatusCode::ACCEPTED);
    let booking = created.booking();
    assert_eq!(booking.status, BookingStatus::Pending);
    assert_eq!(booking.provider.as_str(), "alpha");
    assert_eq!(
        created.headers["location"],
        format!("/v1/bookings/{}", booking.id).as_str()
    );
    assert!(
        created.body.get("customer").is_none(),
        "고객 정보는 응답에 넣지 않는다"
    );

    let fetched = send(
        &app,
        Method::GET,
        &format!("/v1/bookings/{}", booking.id),
        &[("x-api-key", &client.api_key)],
        None,
    )
    .await;
    assert_eq!(fetched.status, StatusCode::OK);
    assert_eq!(fetched.booking(), booking);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 같은_멱등키와_같은_내용이면_같은_예약을_돌려준다(pool: PgPool) {
    let app = common::app(&pool);
    let client = create_client(&pool, "app").await;
    let headers = [
        ("x-api-key", client.api_key.as_str()),
        ("idempotency-key", "order-1"),
    ];

    let slot: DateTime<Utc> = Utc::now() + TimeDelta::days(1);
    let first = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &headers,
        Some(&booking_body(&slot.to_rfc3339())),
    )
    .await;

    // 같은 시각을 한국 시간 표기(+09:00)로 보내도 같은 요청으로 본다
    let kst = FixedOffset::east_opt(9 * 3600).unwrap();
    let same_in_kst = slot.with_timezone(&kst).to_rfc3339();
    assert!(same_in_kst.ends_with("+09:00"));
    let second = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &headers,
        Some(&booking_body(&same_in_kst)),
    )
    .await;

    assert_eq!(first.status, StatusCode::ACCEPTED);
    assert_eq!(second.status, StatusCode::ACCEPTED);
    assert_eq!(first.booking().id, second.booking().id);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 같은_멱등키에_다른_내용이면_422(pool: PgPool) {
    let app = common::app(&pool);
    let client = create_client(&pool, "app").await;
    let headers = [
        ("x-api-key", client.api_key.as_str()),
        ("idempotency-key", "order-1"),
    ];
    let slot = future_slot();

    send(
        &app,
        Method::POST,
        "/v1/bookings",
        &headers,
        Some(&booking_body(&slot)),
    )
    .await;
    let mut different = booking_body(&slot);
    different["party_size"] = json!(4);
    let reply = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &headers,
        Some(&different),
    )
    .await;

    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        reply.body["type"],
        "https://booking.local/problems/idempotency-key-reused"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 요청이_올바르지_않으면_400(pool: PgPool) {
    let app = common::app(&pool);
    let client = create_client(&pool, "app").await;
    let auth = [
        ("x-api-key", client.api_key.as_str()),
        ("idempotency-key", "k"),
    ];

    // Idempotency-Key 없음
    let reply = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &[("x-api-key", &client.api_key)],
        Some(&booking_body(&future_slot())),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.body["type"],
        "https://booking.local/problems/idempotency-key-required"
    );

    // 지난 슬롯
    let past = (Utc::now() - TimeDelta::hours(1)).to_rfc3339();
    let reply = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &auth,
        Some(&booking_body(&past)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.body["detail"],
        "slot_start는 현재 시각 이후여야 합니다"
    );

    // 모르는 필드 (오타)
    let mut typo = booking_body(&future_slot());
    typo["partysize"] = json!(2);
    let reply = send(&app, Method::POST, "/v1/bookings", &auth, Some(&typo)).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.headers["content-type"], "application/problem+json");

    // JSON이 아닌 본문
    let reply = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &auth,
        Some(&json!("not an object")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 없는_업체는_404_쉬는_업체는_409(pool: PgPool) {
    let app = common::app(&pool);
    let client = create_client(&pool, "app").await;
    let auth = [
        ("x-api-key", client.api_key.as_str()),
        ("idempotency-key", "k"),
    ];

    let mut unknown = booking_body(&future_slot());
    unknown["provider"] = json!("gamma");
    let reply = send(&app, Method::POST, "/v1/bookings", &auth, Some(&unknown)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    sqlx::query("UPDATE providers SET enabled = false WHERE id = 'alpha'")
        .execute(&pool)
        .await
        .unwrap();
    let reply = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &auth,
        Some(&booking_body(&future_slot())),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(
        reply.body["type"],
        "https://booking.local/problems/provider-disabled"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 다른_클라이언트의_예약이나_잘못된_id는_404(pool: PgPool) {
    let app = common::app(&pool);
    let owner = create_client(&pool, "owner").await;
    let other = create_client(&pool, "other").await;

    let created = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &[("x-api-key", &owner.api_key), ("idempotency-key", "k")],
        Some(&booking_body(&future_slot())),
    )
    .await
    .booking();

    let reply = send(
        &app,
        Method::GET,
        &format!("/v1/bookings/{}", created.id),
        &[("x-api-key", &other.api_key)],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    let reply = send(
        &app,
        Method::GET,
        "/v1/bookings/not-a-uuid",
        &[("x-api-key", &owner.api_key)],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 헬스_체크(pool: PgPool) {
    let app = common::app(&pool);
    assert_eq!(
        send(&app, Method::GET, "/healthz", &[], None).await.status,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, Method::GET, "/readyz", &[], None).await.status,
        StatusCode::OK
    );
}
