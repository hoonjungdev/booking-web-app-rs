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

/// 회귀 테스트: PostgreSQL은 시각을 마이크로초까지만 저장한다.
/// 나노초가 붙은 시각(Linux의 `Utc::now()`가 이렇다)이 와도 입구에서 마이크로초로 맞추므로,
/// 같은 시각을 마이크로초로 다시 보내면 같은 요청으로 본다 (맞추지 않으면 해시가 달라 422가 난다).
#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 나노초가_붙은_시각은_마이크로초로_맞춰_접수한다(pool: PgPool) {
    let app = common::app(&pool);
    let client = create_client(&pool, "app").await;
    let headers = [
        ("x-api-key", client.api_key.as_str()),
        ("idempotency-key", "order-ns"),
    ];
    let tomorrow = (Utc::now() + TimeDelta::days(1)).date_naive();
    let with_nanos = format!("{tomorrow}T10:00:00.123456789Z");
    let micros_only = format!("{tomorrow}T10:00:00.123456Z");

    let created = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &headers,
        Some(&booking_body(&with_nanos)),
    )
    .await;
    assert_eq!(created.status, StatusCode::ACCEPTED);
    let created = created.booking();
    assert_eq!(
        created.slot_start,
        micros_only.parse::<DateTime<Utc>>().unwrap()
    );

    let replay = send(
        &app,
        Method::POST,
        "/v1/bookings",
        &headers,
        Some(&booking_body(&micros_only)),
    )
    .await;
    assert_eq!(replay.status, StatusCode::ACCEPTED);
    assert_eq!(replay.booking().id, created.id);
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

// ───────────────────────── Phase 6: 빈 시간 조회 ─────────────────────────

mod availability {
    use std::{sync::Arc, time::Duration};

    use axum::http::{Method, StatusCode};
    use booking_server::{
        api::{self, ApiState},
        registry::ProviderRegistry,
    };
    use chrono::{TimeDelta, Utc};
    use db::PgPool;
    use domain::RejectReason;
    use providers::{AvailableSlot, ProviderError, ProviderPolicy};

    use super::common::{
        FakeAdapter, IDEMPOTENT, create_client, resilient, send, single_attempt_policy,
    };

    fn slot() -> AvailableSlot {
        let start = db::normalize_timestamp(Utc::now() + TimeDelta::days(1));
        AvailableSlot {
            start,
            end: start + TimeDelta::hours(1),
            remaining: 3,
        }
    }

    /// 가짜 업체를 alpha 자리에 넣은 API (캐시 TTL은 `ttl_ms`)
    fn app_with(pool: &PgPool, adapter: &Arc<FakeAdapter>, ttl_ms: u64) -> axum::Router {
        let mut registry = ProviderRegistry::default();
        let policy = ProviderPolicy {
            availability_cache_ttl_ms: ttl_ms,
            ..single_attempt_policy()
        };
        registry.insert(resilient(adapter.clone(), policy));
        api::router(ApiState {
            providers: Some(Arc::new(registry)),
            ..ApiState::new(pool.clone())
        })
    }

    #[sqlx::test(migrator = "db::MIGRATOR")]
    async fn 빈_시간은_짧게_캐시하고_업체가_실패하면_예전_값을_stale로_준다(
        pool: PgPool,
    ) {
        let client = create_client(&pool, "app").await;
        let auth = ("x-api-key", client.api_key.as_str());
        let adapter = Arc::new(
            FakeAdapter::new(IDEMPOTENT, vec![]).with_availabilities(vec![
                Ok(vec![slot()]),
                Err(ProviderError::Server { status: 503 }),
                // HTTP 클라이언트의 오류 문구에는 업체 주소가 들어 있다
                Err(ProviderError::Connect {
                    detail: "error sending request for url (http://10.1.2.3/internal)".into(),
                }),
            ]),
        );
        // TTL 100ms: 첫 조회 뒤 바로 다시 물으면 캐시, 조금 지나면 업체에 다시 묻는다
        let app = app_with(&pool, &adapter, 100);
        let uri = "/v1/providers/alpha/availability?resource_id=store-001&date=2026-11-20";

        let fresh = send(&app, Method::GET, uri, &[auth], None).await;
        assert_eq!(fresh.status, StatusCode::OK, "{:?}", fresh.body);
        assert_eq!(fresh.body["stale"], false);
        assert_eq!(fresh.body["slots"][0]["remaining"], 3);
        assert_eq!(fresh.body["date"], "2026-11-20");
        let cached = send(&app, Method::GET, uri, &[auth], None).await;
        assert_eq!(cached.body["fetched_at"], fresh.body["fetched_at"]);
        assert_eq!(
            adapter.availability_calls(),
            1,
            "TTL 안에서는 업체를 부르지 않는다"
        );

        tokio::time::sleep(Duration::from_millis(150)).await;
        let stale = send(&app, Method::GET, uri, &[auth], None).await;
        assert_eq!(stale.status, StatusCode::OK, "{:?}", stale.body);
        assert_eq!(stale.body["stale"], true, "업체가 실패해 예전 값을 준다");
        assert_eq!(stale.body["slots"], fresh.body["slots"]);
        assert_eq!(adapter.availability_calls(), 2);

        // 캐시에 없는 날짜는 줄 값이 없다
        let other = "/v1/providers/alpha/availability?resource_id=store-001&date=2026-11-21";
        let unavailable = send(&app, Method::GET, other, &[auth], None).await;
        assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            unavailable.body["type"],
            "https://booking.local/problems/provider-unavailable"
        );
        let detail = unavailable.body["detail"].as_str().unwrap();
        assert!(
            detail.contains("connect_error") && !detail.contains("10.1.2.3"),
            "클라이언트에게는 결과 분류만 알리고 내부 주소는 내보내지 않는다: {detail}"
        );
    }

    #[sqlx::test(migrator = "db::MIGRATOR")]
    async fn 없는_자원과_잘못된_조건은_캐시로_덮지_않고_그대로_알린다(
        pool: PgPool,
    ) {
        let client = create_client(&pool, "app").await;
        let auth = ("x-api-key", client.api_key.as_str());
        let adapter = Arc::new(
            FakeAdapter::new(IDEMPOTENT, vec![]).with_availabilities(vec![Err(
                ProviderError::Rejected {
                    reason: RejectReason::ResourceNotFound,
                },
            )]),
        );
        let app = app_with(&pool, &adapter, 30_000);

        let missing = send(
            &app,
            Method::GET,
            "/v1/providers/alpha/availability?resource_id=store-999&date=2026-11-20",
            &[auth],
            None,
        )
        .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        assert_eq!(
            missing.body["type"],
            "https://booking.local/problems/resource-not-found"
        );

        for bad in [
            "/v1/providers/alpha/availability?resource_id=store-001&date=20261120",
            "/v1/providers/alpha/availability?date=2026-11-20",
            "/v1/providers/alpha/availability?resource_id=%20&date=2026-11-20",
        ] {
            let reply = send(&app, Method::GET, bad, &[auth], None).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
        }
        let unknown = send(
            &app,
            Method::GET,
            "/v1/providers/nope/availability?resource_id=x&date=2026-11-20",
            &[auth],
            None,
        )
        .await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);
        let anonymous = send(
            &app,
            Method::GET,
            "/v1/providers/alpha/availability?resource_id=x&date=2026-11-20",
            &[],
            None,
        )
        .await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrator = "db::MIGRATOR")]
    async fn 업체_목록은_사용_중인_업체만_보여_주고_파이프라인이_없으면_빈_시간은_503(
        pool: PgPool,
    ) {
        sqlx::query("UPDATE providers SET enabled = false WHERE id = 'beta'")
            .execute(&pool)
            .await
            .unwrap();
        let client = create_client(&pool, "app").await;
        let auth = ("x-api-key", client.api_key.as_str());
        let app = api::router(ApiState::new(pool.clone()));

        let providers = send(&app, Method::GET, "/v1/providers", &[auth], None).await;
        assert_eq!(providers.status, StatusCode::OK);
        assert_eq!(
            providers.body,
            serde_json::json!([{ "id": "alpha", "display_name": "Alpha (mock)" }])
        );
        let disabled = send(
            &app,
            Method::GET,
            "/v1/providers/beta/availability?resource_id=GN01&date=2026-11-20",
            &[auth],
            None,
        )
        .await;
        assert_eq!(disabled.status, StatusCode::CONFLICT);
        // 업체 파이프라인은 워커와 같은 프로세스에만 있다 (--role api)
        let no_runtime = send(
            &app,
            Method::GET,
            "/v1/providers/alpha/availability?resource_id=store-001&date=2026-11-20",
            &[auth],
            None,
        )
        .await;
        assert_eq!(no_runtime.status, StatusCode::SERVICE_UNAVAILABLE);
    }
}
