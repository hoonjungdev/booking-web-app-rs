//! 관리자 API 테스트 (docs/04-api.md §3)
//!
//! 예약은 예약 API로 접수하고, 업체 응답은 가짜 어댑터로 정해 둔 뒤 워커를 `process_next`로 한 단계씩 돌린다.
//! 관리자 API와 워커가 같은 업체 파이프라인(서킷 상태)을 함께 보도록, 둘에 같은 레지스트리를 넣는다.

mod common;

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    http::{Method, StatusCode},
};
use booking_server::{
    api::{self, ApiState},
    chaos::ChaosClient,
    registry::ProviderRegistry,
    web,
    worker::{self, WorkerConfig, WorkerContext},
};
use chrono::{DurationRound, TimeDelta, Utc};
use db::{PgPool, provider_calls::NewProviderCall};
use domain::{BookingId, BookingStatus, ProviderId};
use providers::{ProviderBooking, ProviderError};
use serde_json::{Value, json};

use common::{
    FakeAdapter, IDEMPOTENT, booking_body, create_client, resilient, send, single_attempt_policy,
};

// ───────────────────────── 예약 목록 · 상세 ─────────────────────────

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 예약_목록은_최근_것부터_커서로_이어_읽고_고객_정보를_내보내지_않는다(
    pool: PgPool,
) {
    let ids = [
        accept_booking(&pool, "k1").await,
        accept_booking(&pool, "k2").await,
        accept_booking(&pool, "k3").await,
    ];
    let app = admin_app(ApiState::new(pool.clone())).await;

    let first = get(&app, "/admin/api/bookings?limit=2").await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(item_ids(&first.body), vec![ids[2], ids[1]]);
    let cursor = first.body["next_cursor"]
        .as_str()
        .expect("다음 페이지가 있다");
    assert!(
        !first.body.to_string().contains("홍길동"),
        "고객 정보를 내보내지 않는다"
    );

    let second = get(
        &app,
        &format!("/admin/api/bookings?limit=2&cursor={cursor}"),
    )
    .await;
    assert_eq!(item_ids(&second.body), vec![ids[0]]);
    assert_eq!(second.body["next_cursor"], Value::Null);

    let pending = get(&app, "/admin/api/bookings?status=pending&provider=alpha").await;
    assert_eq!(item_ids(&pending.body).len(), 3);
    let failed = get(&app, "/admin/api/bookings?status=failed").await;
    assert!(item_ids(&failed.body).is_empty());
    let future = (Utc::now() + TimeDelta::hours(1)).format("%Y-%m-%dT%H:%M:%SZ");
    let none = get(&app, &format!("/admin/api/bookings?from={future}")).await;
    assert!(item_ids(&none.body).is_empty());

    for bad in [
        "/admin/api/bookings?status=nope",
        "/admin/api/bookings?cursor=not-a-uuid",
        "/admin/api/bookings?limit=0",
    ] {
        let reply = get(&app, bad).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(reply.headers["content-type"], "application/problem+json");
    }
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 예약_상세는_상태_이력과_호출_기록과_작업을_함께_보여준다(
    pool: PgPool,
) {
    let id = accept_booking(&pool, "k1").await;
    let adapter = Arc::new(FakeAdapter::new(IDEMPOTENT, vec![confirmed("rsv_1")]));
    let (_registry, ctx) = pipeline(&pool, &adapter);
    assert!(worker::process_next(&ctx).await.unwrap());
    db::provider_calls::insert_batch(&pool, &[call_record(Some(id), "success", 80)])
        .await
        .unwrap();

    let app = admin_app(ApiState::new(pool.clone())).await;
    let reply = get(&app, &format!("/admin/api/bookings/{id}")).await;
    assert_eq!(reply.status, StatusCode::OK);
    let detail = &reply.body;
    assert_eq!(detail["booking"]["status"], "confirmed");
    assert_eq!(detail["booking"]["provider_booking_id"], "rsv_1");
    let path: Vec<_> = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["to_status"].as_str().unwrap(),
                e["actor"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(path, vec![("pending", "api"), ("confirmed", "worker")]);
    assert_eq!(detail["events"][0]["from_status"], Value::Null);
    assert_eq!(detail["calls"][0]["outcome"], "success");
    assert_eq!(detail["calls"][0]["duration_ms"], 80);
    assert_eq!(detail["jobs"][0]["kind"], "submit_booking");
    assert_eq!(detail["jobs"][0]["status"], "done");
    assert!(!detail.to_string().contains("010-0000-0000"));

    let missing = get(
        &app,
        &format!("/admin/api/bookings/{}", uuid::Uuid::now_v7()),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let malformed = get(&app, "/admin/api/bookings/not-a-uuid").await;
    assert_eq!(malformed.status, StatusCode::NOT_FOUND);
}

// ───────────────────────── 재시도 · 재큐잉 ─────────────────────────

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 실패한_예약을_재시도하면_dead_작업을_되살리고_업체에_있으면_보내지_않고_확정한다(
    pool: PgPool,
) {
    let id = accept_booking(&pool, "k1").await;
    // 첫 제출은 예상하지 못한 4xx로 포기 → 예약 failed, 작업 dead
    let adapter = Arc::new(
        FakeAdapter::new(IDEMPOTENT, vec![client_error()]).with_finds(vec![found("rsv_9")]),
    );
    let (registry, ctx) = pipeline(&pool, &adapter);
    worker::process_next(&ctx).await.unwrap();
    assert_eq!(status(&pool, id).await, BookingStatus::Failed);
    let dead_job = only_job(&pool, id).await;
    assert_eq!(dead_job.status, "dead");

    let app = admin_app(state_with(&pool, &registry)).await;
    let reply = post(&app, &format!("/admin/api/bookings/{id}/retry"), None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.body);
    assert_eq!(reply.body["booking"]["status"], "pending");
    assert_eq!(
        reply.body["job"]["id"], dead_job.id,
        "새 작업을 만들지 않고 dead 작업을 되살린다"
    );
    assert_eq!(reply.body["job"]["status"], "queued");
    // 시도 횟수는 lease 증표라 되돌리지 않고, 한도를 지금부터 다시 준다
    assert_eq!(reply.body["job"]["attempts"], dead_job.attempts);
    assert_eq!(
        reply.body["job"]["max_attempts"],
        dead_job.attempts + db::jobs::DEFAULT_MAX_ATTEMPTS
    );
    assert_eq!(
        reply.body["job"]["payload"],
        json!({ "lookup_first": true })
    );

    let events = db::bookings::events(&pool, id).await.unwrap();
    let last = events.last().unwrap();
    assert_eq!(
        (last.from_status, last.to_status),
        (Some(BookingStatus::Failed), BookingStatus::Pending)
    );
    assert!(
        last.actor.starts_with("admin:admin-"),
        "조작한 관리자의 이름이 남는다: {}",
        last.actor
    );

    // 워커는 보내기 전에 업체에서 찾아보고, 이미 있으므로 다시 보내지 않는다
    assert!(worker::process_next(&ctx).await.unwrap());
    let booking = db::bookings::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(booking.status, BookingStatus::Confirmed);
    assert_eq!(booking.provider_booking_id.as_deref(), Some("rsv_9"));
    assert_eq!(adapter.create_calls(), 1, "다시 보내지 않았다");
    assert_eq!(adapter.find_calls(), 1);
    assert_eq!(only_job(&pool, id).await.status, "done");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 재시도한_예약이_업체에_없으면_다시_제출한다(pool: PgPool) {
    let id = accept_booking(&pool, "k1").await;
    let adapter = Arc::new(
        FakeAdapter::new(IDEMPOTENT, vec![client_error(), confirmed("rsv_2")])
            .with_finds(vec![Ok(None)]),
    );
    let (registry, ctx) = pipeline(&pool, &adapter);
    worker::process_next(&ctx).await.unwrap();

    let app = admin_app(state_with(&pool, &registry)).await;
    let reply = post(&app, &format!("/admin/api/bookings/{id}/retry"), None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);

    worker::process_next(&ctx).await.unwrap();
    assert_eq!(status(&pool, id).await, BookingStatus::Confirmed);
    assert_eq!((adapter.find_calls(), adapter.create_calls()), (1, 2));
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 기한이_지나_실패한_예약은_재시도할_때_기한을_다시_잡는다(
    pool: PgPool,
) {
    // 장애가 길어지면 예약은 업체를 부르지 못한 채 확정 기한이 지나 실패한다 (가장 흔한 실패)
    let id = accept_booking(&pool, "k1").await;
    sqlx::query("UPDATE bookings SET deadline_at = now() - interval '1 second' WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
    let adapter =
        Arc::new(FakeAdapter::new(IDEMPOTENT, vec![confirmed("rsv_4")]).with_finds(vec![Ok(None)]));
    let (registry, ctx) = pipeline(&pool, &adapter);
    worker::process_next(&ctx).await.unwrap();
    let failed = db::bookings::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(failed.status, BookingStatus::Failed);
    assert_eq!(failed.status_reason.as_deref(), Some("deadline_exceeded"));
    assert_eq!(
        adapter.create_calls(),
        0,
        "기한이 지나 업체를 부르지 않았다"
    );

    let app = admin_app(state_with(&pool, &registry)).await;
    let reply = post(&app, &format!("/admin/api/bookings/{id}/retry"), None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.body);
    let retried = db::bookings::get(&pool, id).await.unwrap().unwrap();
    assert!(
        retried.deadline_at > Utc::now() + TimeDelta::minutes(29),
        "재시도한 시각부터 30분을 다시 준다: {}",
        retried.deadline_at
    );

    worker::process_next(&ctx).await.unwrap();
    assert_eq!(status(&pool, id).await, BookingStatus::Confirmed);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 슬롯이_이미_시작한_예약은_재시도할_수_없다(pool: PgPool) {
    let id = accept_booking(&pool, "k1").await;
    // 테스트 준비: 슬롯이 지난 실패한 예약 (상태를 직접 바꾸는 것은 준비 코드에서만)
    sqlx::query(
        "UPDATE bookings SET status = 'failed', slot_start = now() - interval '1 hour' WHERE id = $1",
    )
    .bind(id.as_uuid())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET status = 'dead' WHERE booking_id = $1")
        .bind(id.as_uuid())
        .execute(&pool)
        .await
        .unwrap();

    let app = admin_app(ApiState::new(pool.clone())).await;
    let reply = post(&app, &format!("/admin/api/bookings/{id}/retry"), None).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(reply.body["detail"].as_str().unwrap().contains("슬롯"));
    assert_eq!(
        status(&pool, id).await,
        BookingStatus::Failed,
        "아무것도 바꾸지 않는다"
    );
    assert_eq!(only_job(&pool, id).await.status, "dead");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn failed가_아닌_예약은_재시도할_수_없다(pool: PgPool) {
    let id = accept_booking(&pool, "k1").await;
    let app = admin_app(ApiState::new(pool.clone())).await;

    let reply = post(&app, &format!("/admin/api/bookings/{id}/retry"), None).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(
        reply.body["type"],
        "https://booking.local/problems/conflict"
    );
    assert!(reply.body["detail"].as_str().unwrap().contains("pending"));

    let missing = post(
        &app,
        &format!("/admin/api/bookings/{}/retry", uuid::Uuid::now_v7()),
        None,
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn dead_작업을_다시_넣으면_실패한_예약도_다시_처리된다(pool: PgPool) {
    let id = accept_booking(&pool, "k1").await;
    let adapter = Arc::new(
        FakeAdapter::new(IDEMPOTENT, vec![client_error(), confirmed("rsv_3")])
            .with_finds(vec![Ok(None)]),
    );
    let (registry, ctx) = pipeline(&pool, &adapter);
    worker::process_next(&ctx).await.unwrap();
    let dead_job = only_job(&pool, id).await;

    let app = admin_app(state_with(&pool, &registry)).await;
    let dead = get(&app, "/admin/api/jobs?status=dead").await;
    assert_eq!(dead.body["items"][0]["id"], dead_job.id);
    let counts = get(&app, "/admin/api/jobs/counts").await;
    assert_eq!(counts.body["total"]["dead"], 1);

    let reply = post(
        &app,
        &format!("/admin/api/jobs/{}/requeue", dead_job.id),
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.body);
    assert_eq!(reply.body["booking"]["status"], "pending");
    assert_eq!(reply.body["job"]["status"], "queued");
    let counts = get(&app, "/admin/api/jobs/counts").await;
    assert_eq!(
        counts.body["total"]["dead"], 0,
        "dead 작업 수가 바로 줄어든다"
    );
    assert_eq!(counts.body["total"]["ready"], 1);

    // 이미 되살린 작업은 다시 넣을 수 없다
    let again = post(
        &app,
        &format!("/admin/api/jobs/{}/requeue", dead_job.id),
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    worker::process_next(&ctx).await.unwrap();
    assert_eq!(status(&pool, id).await, BookingStatus::Confirmed);

    let missing = post(&app, "/admin/api/jobs/999999/requeue", None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 예약이_처리_중인_채로_묻힌_제출_작업은_업체에서_먼저_찾고_기한을_다시_잡는다(
    pool: PgPool,
) {
    let id = accept_booking(&pool, "k1").await;
    let job = only_job(&pool, id).await;
    // 테스트 준비: 작업만 dead가 되고 예약은 pending으로 남은 경우 (포기하는 중에 예약 상태가 먼저 바뀌어 전이가 충돌하는 등).
    // 확정 기한도 이미 지났다.
    sqlx::query("UPDATE jobs SET status = 'dead' WHERE id = $1")
        .bind(job.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE bookings SET deadline_at = now() - interval '1 second' WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
    let adapter =
        Arc::new(FakeAdapter::new(IDEMPOTENT, vec![confirmed("rsv_5")]).with_finds(vec![Ok(None)]));
    let (registry, ctx) = pipeline(&pool, &adapter);

    let app = admin_app(state_with(&pool, &registry)).await;
    let reply = post(&app, &format!("/admin/api/jobs/{}/requeue", job.id), None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.body);
    assert_eq!(
        reply.body["booking"],
        Value::Null,
        "예약 상태는 바꾸지 않는다"
    );
    assert_eq!(
        reply.body["job"]["payload"],
        json!({ "lookup_first": true })
    );
    let booking = db::bookings::get(&pool, id).await.unwrap().unwrap();
    assert!(booking.deadline_at > Utc::now(), "지난 기한을 다시 잡는다");

    worker::process_next(&ctx).await.unwrap();
    assert_eq!(status(&pool, id).await, BookingStatus::Confirmed);
    assert_eq!(
        (adapter.find_calls(), adapter.create_calls()),
        (1, 1),
        "먼저 찾아보고 없으니 보냈다"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 예약과_상관없이_묻힌_작업은_작업만_되살린다(pool: PgPool) {
    let id = accept_booking(&pool, "k1").await;
    let job = only_job(&pool, id).await;
    // 이 서버가 모르는 종류의 작업이라 워커가 묻었다고 친다
    sqlx::query("UPDATE jobs SET kind = 'from_the_future', status = 'dead' WHERE id = $1")
        .bind(job.id)
        .execute(&pool)
        .await
        .unwrap();

    let app = admin_app(ApiState::new(pool.clone())).await;
    let reply = post(&app, &format!("/admin/api/jobs/{}/requeue", job.id), None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.body);
    assert_eq!(reply.body["booking"], Value::Null, "예약 상태는 그대로다");
    assert_eq!(reply.body["job"]["kind"], "from_the_future");
    assert_eq!(status(&pool, id).await, BookingStatus::Pending);
}

// ───────────────────────── 업체 · 서킷 ─────────────────────────

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 업체_목록과_상세는_정책과_런타임_상태를_보여준다(pool: PgPool) {
    let without_runtime = admin_app(ApiState::new(pool.clone())).await;
    let list = get(&without_runtime, "/admin/api/providers").await;
    assert_eq!(list.body[0]["id"], "alpha");
    assert_eq!(
        list.body[0]["runtime"],
        Value::Null,
        "업체 파이프라인이 없는 프로세스"
    );

    // 파이프라인이 없으면 DB의 정책을 보여 준다
    let stored = get(&without_runtime, "/admin/api/providers/alpha").await;
    assert_eq!(stored.body["policy"]["circuit_breaker"]["window"], 20);
    assert_eq!(stored.body["restart_required"], false);

    // 파이프라인이 있으면 실제로 적용 중인 정책을 보여 준다. 테스트의 파이프라인은 DB와 다른 정책
    // (호출 내 재시도 없음, 잘 열리지 않는 서킷)으로 만들었으므로, 서버를 다시 시작해야 DB 값이 반영되는 상황과 같다.
    let adapter = Arc::new(FakeAdapter::new(IDEMPOTENT, vec![]));
    let (registry, _ctx) = pipeline(&pool, &adapter);
    let app = admin_app(state_with(&pool, &registry)).await;
    let detail = get(&app, "/admin/api/providers/alpha").await;
    assert_eq!(detail.status, StatusCode::OK);
    assert_eq!(detail.body["policy"]["attempt_timeout_ms"], 3000);
    assert_eq!(detail.body["policy"]["retry"]["max_attempts"], 1);
    assert_eq!(detail.body["policy"]["circuit_breaker"]["window"], 1000);
    assert_eq!(detail.body["restart_required"], true);
    let runtime = &detail.body["provider"]["runtime"];
    assert_eq!(runtime["circuit"]["state"], "closed");
    assert_eq!(runtime["circuit"]["forced"], Value::Null);
    assert_eq!(runtime["in_flight"], 0);

    let missing = get(&app, "/admin/api/providers/nope").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 서킷을_강제로_열면_워커가_업체를_부르지_않고_미룬다(pool: PgPool) {
    let adapter = Arc::new(FakeAdapter::new(IDEMPOTENT, vec![confirmed("rsv_1")]));
    let (registry, ctx) = pipeline(&pool, &adapter);
    let app = admin_app(state_with(&pool, &registry)).await;

    let opened = post(
        &app,
        "/admin/api/providers/alpha/circuit",
        Some(&json!({ "action": "force_open" })),
    )
    .await;
    assert_eq!(opened.status, StatusCode::OK, "{:?}", opened.body);
    assert_eq!(opened.body["circuit"]["state"], "open");
    assert_eq!(opened.body["circuit"]["forced"], "open");

    let id = accept_booking(&pool, "k1").await;
    worker::process_next(&ctx).await.unwrap();
    assert_eq!(
        adapter.create_calls(),
        0,
        "열린 서킷 뒤로 요청이 나가지 않는다"
    );
    assert_eq!(status(&pool, id).await, BookingStatus::Pending);
    let job = only_job(&pool, id).await;
    assert_eq!(
        (job.status.as_str(), job.attempts),
        ("queued", 0),
        "시도 횟수를 쓰지 않고 미룬다"
    );

    let reset = post(
        &app,
        "/admin/api/providers/alpha/circuit",
        Some(&json!({ "action": "reset" })),
    )
    .await;
    assert_eq!(reset.body["circuit"]["state"], "closed");
    assert_eq!(reset.body["circuit"]["forced"], Value::Null);

    let bad = post(
        &app,
        "/admin/api/providers/alpha/circuit",
        Some(&json!({ "action": "explode" })),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    // 업체 파이프라인이 없는 프로세스(--role api)에서는 조작할 수 없다
    let without_runtime = admin_app(ApiState::new(pool.clone())).await;
    let unavailable = post(
        &without_runtime,
        "/admin/api/providers/alpha/circuit",
        Some(&json!({ "action": "reset" })),
    )
    .await;
    assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        unavailable.body["detail"]
            .as_str()
            .unwrap()
            .contains("--role all")
    );
}

// ───────────────────────── 통계 ─────────────────────────

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 통계는_구간을_빠짐없이_채우고_막힌_시도를_지연에서_뺀다(
    pool: PgPool,
) {
    // 구간(1분) 경계에 맞춘다. 맞추지 않으면 시작 시각의 초가 52초 이후일 때 호출 4건이 두 구간으로 나뉜다
    let start = (Utc::now() - TimeDelta::minutes(30))
        .duration_trunc(TimeDelta::minutes(1))
        .expect("분 단위로 자를 수 있다");
    let calls = vec![
        NewProviderCall {
            started_at: start + TimeDelta::seconds(5),
            ..call_record(None, "success", 100)
        },
        NewProviderCall {
            started_at: start + TimeDelta::seconds(6),
            ..call_record(None, "rejected", 300)
        },
        NewProviderCall {
            started_at: start + TimeDelta::seconds(7),
            ..call_record(None, "circuit_open", 0)
        },
        NewProviderCall {
            started_at: start + TimeDelta::seconds(8),
            ..call_record(None, "rate_limited", 20)
        },
    ];
    db::provider_calls::insert_batch(&pool, &calls)
        .await
        .unwrap();
    let app = admin_app(ApiState::new(pool.clone())).await;

    let from = start.format("%Y-%m-%dT%H:%M:%SZ");
    let to = (start + TimeDelta::minutes(5)).format("%Y-%m-%dT%H:%M:%SZ");
    let reply = get(
        &app,
        &format!("/admin/api/stats/providers/alpha/timeseries?from={from}&to={to}&interval=1m"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let points = reply.body["points"].as_array().unwrap();
    assert!(
        points.len() >= 5,
        "호출이 없는 구간도 들어 있다: {}",
        points.len()
    );
    let busy: Vec<_> = points.iter().filter(|p| p["calls"] != 0).collect();
    assert_eq!(busy.len(), 1);
    assert_eq!(busy[0]["calls"], 4);
    assert_eq!(busy[0]["by_outcome"]["circuit_open"], 1);
    // 부른 시도(100, 300, 20)만으로 계산한다
    assert_eq!(busy[0]["latency_ms"]["p50"], 100.0);
    let empty = points.iter().find(|p| p["calls"] == 0).unwrap();
    assert_eq!(empty["latency_ms"]["p50"], Value::Null);

    let summary = get(&app, "/admin/api/stats/providers/summary?window=1h").await;
    let alpha = &summary.body["providers"][0];
    assert_eq!(alpha["provider"], "alpha");
    assert_eq!(alpha["attempts"], 4);
    assert_eq!(alpha["called"], 3);
    assert_eq!(alpha["blocked"], 1);
    assert_eq!(alpha["rate_limited"], 1);
    // 정상 응답(success + rejected) 2 ÷ 부른 시도 3
    let rate = alpha["success_rate"].as_f64().unwrap();
    assert!((rate - 2.0 / 3.0).abs() < 1e-9, "{rate}");

    for bad in [
        "/admin/api/stats/providers/alpha/timeseries?interval=2m".to_owned(),
        format!("/admin/api/stats/providers/alpha/timeseries?from={to}&to={from}"),
        "/admin/api/stats/providers/alpha/timeseries?from=2026-01-01T00:00:00Z&to=2026-03-01T00:00:00Z&interval=10s".to_owned(),
        "/admin/api/stats/providers/summary?window=7d".to_owned(),
    ] {
        assert_eq!(get(&app, &bad).await.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let missing = get(&app, "/admin/api/stats/providers/nope/timeseries").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 끝_시각이_구간_경계_바로_뒤여도_마지막_구간을_빠뜨리지_않는다(
    pool: PgPool,
) {
    // 화면은 "지금"을 밀리초까지 보낸다. 끝 시각이 구간 경계보다 0.5초 늦으면 그 0.5초의 호출이 마지막 구간에 들어가야 한다.
    let boundary: chrono::DateTime<Utc> = "2026-11-10T03:05:00Z".parse().unwrap();
    let call = NewProviderCall {
        started_at: boundary + TimeDelta::milliseconds(200),
        ..call_record(None, "success", 40)
    };
    db::provider_calls::insert_batch(&pool, &[call])
        .await
        .unwrap();

    let app = admin_app(ApiState::new(pool.clone())).await;
    let reply = get(
        &app,
        "/admin/api/stats/providers/alpha/timeseries?from=2026-11-10T03:00:00Z&to=2026-11-10T03:05:00.500Z&interval=1m",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let points = reply.body["points"].as_array().unwrap();
    assert_eq!(points.len(), 6, "03:00 ~ 03:05 구간 6개");
    let last = points.last().unwrap();
    assert_eq!(last["bucket"], "2026-11-10T03:05:00Z");
    assert_eq!(last["calls"], 1);
}

// ───────────────────────── 장애 주입 중계 ─────────────────────────

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 장애_주입_요청을_mock_업체에_중계한다(pool: PgPool) {
    let mock = start_mock().await;
    let state = ApiState {
        chaos: Some(ChaosClient::new(&mock).expect("클라이언트를 만들 수 있어야 한다")),
        ..ApiState::new(pool.clone())
    };
    let app = admin_app(state).await;

    let current = get(&app, "/admin/api/chaos/alpha").await;
    assert_eq!(current.status, StatusCode::OK, "{:?}", current.body);
    assert_eq!(current.body["outage"], false);

    let outage = post(&app, "/admin/api/chaos/alpha/preset/outage", None).await;
    assert_eq!(outage.status, StatusCode::OK);
    assert_eq!(outage.body["outage"], true);
    assert_eq!(
        get(&app, "/admin/api/chaos/alpha").await.body["outage"],
        true
    );

    let mut settings = current.body.clone();
    settings["error"]["probability"] = json!(0.5);
    let changed = put(&app, "/admin/api/chaos/alpha", &settings).await;
    assert_eq!(changed.status, StatusCode::OK);
    assert_eq!(changed.body["error"]["probability"], 0.5);

    settings["slow"]["probability"] = json!(1.5);
    let rejected = put(&app, "/admin/api/chaos/alpha", &settings).await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
    assert!(
        rejected.body["detail"]
            .as_str()
            .unwrap()
            .contains("slow.probability")
    );

    // mock 업체에는 alpha와 beta가 있다
    let beta = get(&app, "/admin/api/chaos/beta").await;
    assert_eq!(beta.status, StatusCode::OK, "{:?}", beta.body);
    let unknown_provider = get(&app, "/admin/api/chaos/gamma").await;
    assert_eq!(unknown_provider.status, StatusCode::NOT_FOUND);
    let unknown_preset = post(&app, "/admin/api/chaos/alpha/preset/meteor", None).await;
    assert_eq!(unknown_preset.status, StatusCode::BAD_REQUEST);

    // 설정하지 않았으면 꺼져 있다
    let disabled = admin_app(ApiState::new(pool.clone())).await;
    let reply = get(&disabled, "/admin/api/chaos/alpha").await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        reply.body["detail"]
            .as_str()
            .unwrap()
            .contains("BOOKING_CHAOS_URL")
    );
}

// ───────────────────────── 화면 설정 · 문서 · 정적 파일 ─────────────────────────

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 화면_설정은_쓸_수_있는_기능을_알려_준다(pool: PgPool) {
    let adapter = Arc::new(FakeAdapter::new(IDEMPOTENT, vec![]));
    let (registry, _ctx) = pipeline(&pool, &adapter);
    let state = ApiState {
        grafana_url: Some("http://localhost:3000".into()),
        ..state_with(&pool, &registry)
    };
    let info = get(&admin_app(state).await, "/admin/api/info").await;
    assert_eq!(info.body["grafana_url"], "http://localhost:3000");
    assert_eq!(info.body["chaos_enabled"], false);
    assert_eq!(info.body["provider_runtime"], true);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn api_문서와_swagger_ui를_제공한다(pool: PgPool) {
    let app = admin_app(ApiState::new(pool.clone())).await;
    let spec = get(&app, "/openapi.json").await;
    assert_eq!(spec.status, StatusCode::OK);
    assert_eq!(spec.body["openapi"], "3.1.0");
    assert!(spec.body["paths"]["/admin/api/bookings/{id}/retry"]["post"].is_object());
    assert_eq!(spec.body, serde_json::to_value(api::openapi()).unwrap());

    let docs = get(&app, "/docs/").await;
    assert_eq!(docs.status, StatusCode::OK);
    assert!(docs.body.as_str().unwrap().contains("swagger"));

    let unknown = get(&app, "/admin/api/nope").await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.headers["content-type"], "application/problem+json");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 관리자_웹은_없는_화면_경로에도_index_html을_돌려주고_api_경로는_건드리지_않는다(
    pool: PgPool,
) {
    let dir = std::env::temp_dir().join(format!("admin-web-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(
        dir.join("index.html"),
        "<!doctype html><div id=\"root\"></div>",
    )
    .unwrap();
    std::fs::write(dir.join("assets/app.js"), "console.log('admin')").unwrap();

    let app = api::router(ApiState::new(pool.clone()))
        .merge(web::routes(&dir).expect("index.html이 있으면 화면 경로를 만든다"));

    for path in ["/admin/", "/admin", "/admin/bookings/0193"] {
        let page = get_raw(&app, path).await;
        assert_eq!(page.status, StatusCode::OK, "{path}");
        assert!(page.body.as_str().unwrap().contains("root"), "{path}");
    }
    let script = get_raw(&app, "/admin/assets/app.js").await;
    assert!(
        script.headers["content-type"]
            .to_str()
            .unwrap()
            .contains("javascript")
    );
    let api = get_raw(&app, "/admin/api/info").await;
    assert_eq!(
        api.body["chaos_enabled"], false,
        "API 경로는 화면 파일보다 먼저 맞는다"
    );
    let unknown = get_raw(&app, "/admin/api/nope").await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let root = get_raw(&app, "/").await;
    assert_eq!(root.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(root.headers["location"], "/admin/");

    assert!(
        web::routes(&dir.join("missing")).is_none(),
        "빌드 결과가 없으면 화면을 열지 않는다"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ───────────────────────── Phase 6: 로그인 · 권한 ─────────────────────────

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 로그인하면_세션_쿠키를_주고_로그아웃하면_세션이_끝난다(
    pool: PgPool,
) {
    let hash = booking_server::admin_auth::hash_password("correct horse".into())
        .await
        .unwrap();
    db::admin::create_user(&pool, "kim", &hash, "operator")
        .await
        .unwrap();
    let app = api::router(ApiState::new(pool.clone()));

    let login = |username: &'static str, password: &'static str| {
        let app = app.clone();
        async move {
            send(
                &app,
                Method::POST,
                "/admin/api/login",
                &[],
                Some(&json!({ "username": username, "password": password })),
            )
            .await
        }
    };
    let ok = login("kim", "correct horse").await;
    assert_eq!(ok.status, StatusCode::OK, "{:?}", ok.body);
    assert_eq!(ok.body["username"], "kim");
    assert_eq!(ok.body["role"], "operator");
    let set_cookie = ok.headers["set-cookie"].to_str().unwrap().to_owned();
    for part in [
        "booking_admin_session=",
        "HttpOnly",
        "SameSite=Strict",
        "Path=/admin",
    ] {
        assert!(set_cookie.contains(part), "{set_cookie}");
    }
    let cookie = set_cookie.split(';').next().unwrap().to_owned();

    let session = send(
        &app,
        Method::GET,
        "/admin/api/session",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(session.status, StatusCode::OK);
    assert_eq!(session.body["username"], "kim");
    let entries = db::admin::audit_log(&pool, None, 10).await.unwrap();
    assert_eq!(
        (entries[0].actor.as_str(), entries[0].action.as_str()),
        ("admin:kim", "admin.login")
    );

    // 틀린 비밀번호와 없는 계정은 같은 오류다 (계정이 있는지 알려 주지 않는다)
    let wrong = login("kim", "wrong").await;
    let nobody = login("nobody", "correct horse").await;
    for reply in [&wrong, &nobody] {
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            reply.body["type"],
            "https://booking.local/problems/login-failed"
        );
        assert!(reply.headers.get("set-cookie").is_none());
    }

    let logout = send(
        &app,
        Method::POST,
        "/admin/api/logout",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);
    assert!(
        logout.headers["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    let after = send(
        &app,
        Method::GET,
        "/admin/api/session",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 로그인하지_않으면_관리자_api를_쓸_수_없고_역할에_따라_조작이_막힌다(
    pool: PgPool,
) {
    let anonymous = api::router(ApiState::new(pool.clone()));
    for uri in [
        "/admin/api/bookings",
        "/admin/api/providers",
        "/admin/api/audit",
    ] {
        let reply = get_raw(&anonymous, uri).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(
            reply.body["type"],
            "https://booking.local/problems/login-required"
        );
    }
    // 화면 설정은 로그인 전에도 읽는다 (로그인 화면이 쓴다)
    assert_eq!(
        get_raw(&anonymous, "/admin/api/info").await.status,
        StatusCode::OK
    );

    let id = accept_booking(&pool, "k1").await;
    let viewer = as_role(ApiState::new(pool.clone()), "viewer").await;
    assert_eq!(
        get(&viewer, "/admin/api/bookings").await.status,
        StatusCode::OK
    );
    for (uri, body) in [
        (format!("/admin/api/bookings/{id}/retry"), None),
        (
            "/admin/api/providers/alpha/circuit".to_owned(),
            Some(json!({ "action": "reset" })),
        ),
        ("/admin/api/providers/alpha/reconcile".to_owned(), None),
    ] {
        let reply = post(&viewer, &uri, body.as_ref()).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{uri}");
        assert!(reply.body["detail"].as_str().unwrap().contains("operator"));
    }

    // 정책 변경은 admin만
    let operator = as_role(ApiState::new(pool.clone()), "operator").await;
    let policy = get(&operator, "/admin/api/providers/alpha").await.body["policy"].clone();
    let denied = put(&operator, "/admin/api/providers/alpha/policy", &policy).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert!(denied.body["detail"].as_str().unwrap().contains("admin"));
}

// ───────────────────────── Phase 6: 정책 실시간 변경 ─────────────────────────

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 정책을_바꾸면_저장하고_실행_중인_파이프라인에_바로_반영한다(
    pool: PgPool,
) {
    let adapter = Arc::new(FakeAdapter::new(IDEMPOTENT, vec![]));
    let (registry, _ctx) = pipeline(&pool, &adapter);
    let state = ApiState {
        worker_lease: Some(Duration::from_secs(60)),
        ..state_with(&pool, &registry)
    };
    let app = admin_app(state).await;
    let before = get(&app, "/admin/api/providers/alpha").await.body;
    assert_eq!(before["provider"]["runtime"]["rate_limit_allowed"], 10_000);

    let mut policy = before["policy"].clone();
    policy["rate_limit"]["per_second"] = json!(3);
    policy["rate_limit"]["burst"] = json!(2);
    policy["bulkhead"]["max_concurrency"] = json!(2);
    policy["availability_cache_ttl_ms"] = json!(5_000);
    let reply = put(&app, "/admin/api/providers/alpha/policy", &policy).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    assert_eq!(reply.body["policy"], policy);
    assert_eq!(
        reply.body["restart_required"], false,
        "DB와 실행 중인 정책이 같다"
    );
    let runtime = &reply.body["provider"]["runtime"];
    assert_eq!(runtime["rate_limit_allowed"], 3);
    assert_eq!(runtime["rate_limit_ceiling"], 3);
    assert_eq!(runtime["limiter_burst"], 2);
    assert_eq!(runtime["max_concurrency"], 2);

    // 파이프라인과 DB 모두 바뀌었다
    let alpha = registry.get(&ProviderId::parse("alpha").unwrap()).unwrap();
    assert_eq!(alpha.policy().rate_limit.per_second, 3);
    let stored = db::providers::find(&pool, &ProviderId::parse("alpha").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.policy["rate_limit"]["per_second"], 3);
    let audit = get(&app, "/admin/api/audit").await;
    let entry = &audit.body["items"][0];
    assert_eq!(entry["action"], "provider.policy.update");
    assert_eq!(entry["target"], "alpha");
    assert_eq!(entry["detail"]["after"]["rate_limit"]["per_second"], 3);
    assert!(entry["actor"].as_str().unwrap().starts_with("admin:"));
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 동작할_수_없거나_lease보다_긴_호출이_되는_정책은_거절한다(
    pool: PgPool,
) {
    let adapter = Arc::new(FakeAdapter::new(IDEMPOTENT, vec![]));
    let (registry, _ctx) = pipeline(&pool, &adapter);
    let state = ApiState {
        worker_lease: Some(Duration::from_secs(60)),
        ..state_with(&pool, &registry)
    };
    let app = admin_app(state).await;
    let policy = get(&app, "/admin/api/providers/alpha").await.body["policy"].clone();

    let mut broken = policy.clone();
    broken["circuit_breaker"]["failure_rate"] = json!(2.0);
    let reply = put(&app, "/admin/api/providers/alpha/policy", &broken).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(
        reply.body["detail"]
            .as_str()
            .unwrap()
            .contains("failure_rate")
    );

    // 호출 한 번이 최대 59초 + 대기 → lease 60초 - 여유 5초를 넘는다
    let mut too_long = policy.clone();
    too_long["call_budget_ms"] = json!(59_000);
    let reply = put(&app, "/admin/api/providers/alpha/policy", &too_long).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(
        reply.body["detail"].as_str().unwrap().contains("lease"),
        "{:?}",
        reply.body
    );

    let mut unknown_field = policy.clone();
    unknown_field["surprise"] = json!(1);
    let reply = put(&app, "/admin/api/providers/alpha/policy", &unknown_field).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    let missing = put(&app, "/admin/api/providers/nope/policy", &policy).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    // 아무것도 바뀌지 않았다
    let alpha = registry.get(&ProviderId::parse("alpha").unwrap()).unwrap();
    assert_eq!(alpha.policy().call_budget_ms, 15_000);
}

// ───────────────────────── Phase 6: 대사 이슈 ─────────────────────────

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 대사_이슈를_보고_처리_완료로_표시한다(pool: PgPool) {
    use db::reconciliation::{IssueKind, NewIssue};
    let mut conn = pool.acquire().await.unwrap();
    let mut ids = Vec::new();
    for (kind, resolved_by) in [
        (IssueKind::DuplicateSuspected, None),
        (IssueKind::MissingAtProvider, None),
        (IssueKind::StatusMismatch, Some("reconciler")),
    ] {
        let issue = NewIssue {
            provider_id: ProviderId::parse("alpha").unwrap(),
            booking_id: Some(BookingId::from_uuid(uuid::Uuid::now_v7())),
            provider_booking_id: Some("rsv_1".into()),
            kind,
            detail: json!({ "ours": "confirmed" }),
            resolved_by: resolved_by.map(str::to_owned),
        };
        ids.push(
            db::reconciliation::record(&mut conn, &issue)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let app = admin_app(ApiState::new(pool.clone())).await;

    let open = get(&app, "/admin/api/reconciliation/issues").await;
    assert_eq!(open.status, StatusCode::OK, "{:?}", open.body);
    let kinds: Vec<_> = open.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|issue| issue["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec!["missing_at_provider", "duplicate_suspected"],
        "미해결만, 최근 것부터"
    );
    let fixed = get(&app, "/admin/api/reconciliation/issues?resolved=true").await;
    assert_eq!(fixed.body["items"][0]["resolved_by"], "reconciler");
    let counts = get(&app, "/admin/api/reconciliation/counts").await;
    assert_eq!(counts.body["open"], 2);
    assert_eq!(counts.body["by_kind"]["duplicate_suspected"], 1);
    assert_eq!(counts.body["by_kind"]["missing_locally"], 0);

    let resolved = post(
        &app,
        &format!("/admin/api/reconciliation/issues/{}/resolve", ids[0]),
        None,
    )
    .await;
    assert_eq!(resolved.status, StatusCode::OK, "{:?}", resolved.body);
    assert!(
        resolved.body["resolved_by"]
            .as_str()
            .unwrap()
            .starts_with("admin:")
    );
    assert!(resolved.body["resolved_at"].is_string());
    let again = post(
        &app,
        &format!("/admin/api/reconciliation/issues/{}/resolve", ids[0]),
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    let missing = post(
        &app,
        "/admin/api/reconciliation/issues/999999/resolve",
        None,
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(
        get(&app, "/admin/api/reconciliation/counts").await.body["open"],
        1
    );
    let bad = get(&app, "/admin/api/reconciliation/issues?kind=nope").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 지금_대사를_돌리면_대사_작업을_당겨_오고_하나만_둔다(pool: PgPool) {
    let app = admin_app(ApiState::new(pool.clone())).await;
    let first = post(&app, "/admin/api/providers/beta/reconcile", None).await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{:?}", first.body);
    assert_eq!(first.body["kind"], "reconcile_provider");
    assert_eq!(first.body["provider"], "beta");
    assert_eq!(first.body["status"], "queued");
    let again = post(&app, "/admin/api/providers/beta/reconcile", None).await;
    assert_eq!(
        again.body["id"], first.body["id"],
        "업체마다 대사 작업은 하나뿐이다"
    );
    let missing = post(&app, "/admin/api/providers/nope/reconcile", None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 포기한_대사_작업을_다시_넣으면_그_업체의_대사를_지금_돌린다(
    pool: PgPool,
) {
    // 대사 작업은 포기할(dead) 때 같은 트랜잭션에서 다음 정기 대사를 등록한다. 그래서 dead 행 옆에 대기 중인 대사 작업이 늘 있다
    let beta = ProviderId::parse("beta").expect("업체 ID");
    let mut conn = pool.acquire().await.expect("DB 연결");
    let dead = db::jobs::schedule_reconcile(&mut conn, &beta, Duration::ZERO, None)
        .await
        .expect("대사 등록")
        .expect("새 대사 작업");
    sqlx::query("UPDATE jobs SET status = 'dead', last_error = 'reconcile_failed' WHERE id = $1")
        .bind(dead)
        .execute(&pool)
        .await
        .unwrap();
    let next = db::jobs::schedule_reconcile(&mut conn, &beta, Duration::from_secs(600), None)
        .await
        .expect("대사 등록")
        .expect("다음 대사 작업");

    let app = admin_app(ApiState::new(pool.clone())).await;
    let reply = post(&app, &format!("/admin/api/jobs/{dead}/requeue"), None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.body);
    assert_eq!(
        reply.body["job"]["id"], next,
        "업체마다 하나뿐인 대사 작업을 지금으로 당긴다"
    );
    assert_eq!(reply.body["job"]["status"], "queued");
    let (pulled,): (bool,) = sqlx::query_as("SELECT run_at <= now() FROM jobs WHERE id = $1")
        .bind(next)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(pulled, "다음 정기 대사를 기다리지 않는다");
    let closed = db::jobs::get(&mut conn, dead)
        .await
        .unwrap()
        .expect("포기한 대사 작업");
    assert_eq!(
        closed.status, "done",
        "포기한 행은 정리해 dead 알림이 그친다"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 다른_출처의_페이지가_보낸_조작_요청은_거절한다(pool: PgPool) {
    // SameSite=Strict 쿠키는 같은 사이트의 다른 출처(localhost의 다른 포트, 형제 서브도메인)에서 보낸 요청에도 실린다
    let app = admin_app(ApiState::new(pool.clone())).await;
    let uri = "/admin/api/providers/beta/reconcile";

    let same_site = post_with(&app, uri, &[("sec-fetch-site", "same-site")]).await;
    assert_eq!(
        same_site.status,
        StatusCode::FORBIDDEN,
        "{:?}",
        same_site.body
    );
    assert_eq!(
        same_site.body["type"],
        "https://booking.local/problems/cross-origin"
    );
    let other_port = post_with(
        &app,
        uri,
        &[
            ("origin", "http://localhost:3000"),
            ("host", "localhost:8080"),
        ],
    )
    .await;
    assert_eq!(other_port.status, StatusCode::FORBIDDEN);

    // 관리자 화면(같은 출처)에서 보낸 요청, 브라우저가 아닌 클라이언트(헤더 없음)는 받는다
    let same_origin = post_with(&app, uri, &[("sec-fetch-site", "same-origin")]).await;
    assert_eq!(
        same_origin.status,
        StatusCode::ACCEPTED,
        "{:?}",
        same_origin.body
    );
    let matching = post_with(
        &app,
        uri,
        &[
            ("origin", "http://localhost:8080"),
            ("host", "localhost:8080"),
        ],
    )
    .await;
    assert_eq!(matching.status, StatusCode::ACCEPTED);
    assert_eq!(post_with(&app, uri, &[]).await.status, StatusCode::ACCEPTED);
    // 조회는 상태를 바꾸지 않으므로 확인하지 않는다
    let read = send(
        &app.app,
        Method::GET,
        "/admin/api/providers",
        &[("cookie", &app.cookie), ("sec-fetch-site", "cross-site")],
        None,
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
}

/// 쿠키에 헤더를 더해 POST한다
async fn post_with(admin: &Admin, uri: &str, headers: &[(&str, &str)]) -> common::Reply {
    let mut all = vec![("cookie", admin.cookie.as_str())];
    all.extend_from_slice(headers);
    send(&admin.app, Method::POST, uri, &all, None).await
}

// ───────────────────────── 준비 코드 ─────────────────────────

/// 예약 API로 예약 하나를 접수하고 ID를 돌려준다 (`key`: 멱등키)
async fn accept_booking(pool: &PgPool, key: &str) -> BookingId {
    let client = create_client(pool, &format!("app-{key}")).await;
    let slot = (Utc::now() + TimeDelta::days(1)).to_rfc3339();
    send(
        &common::app(pool),
        Method::POST,
        "/v1/bookings",
        &[("x-api-key", &client.api_key), ("idempotency-key", key)],
        Some(&booking_body(&slot)),
    )
    .await
    .booking()
    .id
}

/// 가짜 어댑터를 alpha 자리에 넣은 업체 레지스트리와, 그 레지스트리를 쓰는 워커
fn pipeline(pool: &PgPool, adapter: &Arc<FakeAdapter>) -> (Arc<ProviderRegistry>, WorkerContext) {
    let mut registry = ProviderRegistry::default();
    registry.insert(resilient(adapter.clone(), single_attempt_policy()));
    let registry = Arc::new(registry);
    let ctx = WorkerContext {
        pool: pool.clone(),
        providers: registry.clone(),
        config: WorkerConfig {
            concurrency: 1,
            poll_interval: Duration::from_millis(10),
            lease: Duration::from_secs(60),
            unknown_recheck_interval: Duration::ZERO,
            ..WorkerConfig::default()
        },
    };
    (registry, ctx)
}

/// 워커와 같은 업체 파이프라인을 보는 관리자 API 상태
fn state_with(pool: &PgPool, registry: &Arc<ProviderRegistry>) -> ApiState {
    ApiState {
        providers: Some(registry.clone()),
        ..ApiState::new(pool.clone())
    }
}

/// 관리자 API를 부르는 테스트 클라이언트: 라우터 + 로그인 세션 쿠키
struct Admin {
    app: Router,
    cookie: String,
}

/// admin 역할(모든 권한)로 로그인한 테스트 클라이언트
async fn admin_app(state: ApiState) -> Admin {
    as_role(state, "admin").await
}

/// 주어진 역할로 로그인한 테스트 클라이언트
async fn as_role(state: ApiState, role: &str) -> Admin {
    let cookie = common::admin_cookie(&state.pool, role).await;
    Admin {
        app: api::router(state),
        cookie,
    }
}

async fn get(admin: &Admin, uri: &str) -> common::Reply {
    send(
        &admin.app,
        Method::GET,
        uri,
        &[("cookie", &admin.cookie)],
        None,
    )
    .await
}

async fn post(admin: &Admin, uri: &str, body: Option<&Value>) -> common::Reply {
    send(
        &admin.app,
        Method::POST,
        uri,
        &[("cookie", &admin.cookie)],
        body,
    )
    .await
}

async fn put(admin: &Admin, uri: &str, body: &Value) -> common::Reply {
    send(
        &admin.app,
        Method::PUT,
        uri,
        &[("cookie", &admin.cookie)],
        Some(body),
    )
    .await
}

/// 로그인 없이 라우터를 직접 부른다 (공개 경로 · 정적 파일)
async fn get_raw(app: &Router, uri: &str) -> common::Reply {
    send(app, Method::GET, uri, &[], None).await
}

fn item_ids(page: &Value) -> Vec<BookingId> {
    page["items"]
        .as_array()
        .expect("목록 응답이어야 한다")
        .iter()
        .map(|item| serde_json::from_value(item["id"].clone()).expect("예약 ID"))
        .collect()
}

async fn status(pool: &PgPool, id: BookingId) -> BookingStatus {
    db::bookings::get(pool, id)
        .await
        .expect("예약을 읽을 수 있어야 한다")
        .expect("예약이 있어야 한다")
        .status
}

/// 예약의 작업 (하나뿐이어야 한다: 재시도해도 작업을 새로 만들지 않는다)
async fn only_job(pool: &PgPool, id: BookingId) -> db::jobs::JobRecord {
    let mut jobs = db::jobs::for_booking(pool, id)
        .await
        .expect("작업을 읽을 수 있어야 한다");
    assert_eq!(jobs.len(), 1, "예약에 작업이 하나여야 한다: {jobs:?}");
    jobs.remove(0)
}

fn confirmed(id: &str) -> Result<ProviderBooking, ProviderError> {
    Ok(ProviderBooking {
        provider_booking_id: id.into(),
    })
}

fn found(id: &str) -> Result<Option<ProviderBooking>, ProviderError> {
    Ok(Some(ProviderBooking {
        provider_booking_id: id.into(),
    }))
}

/// 다시 보내도 같은 결과일 예상하지 못한 4xx (워커가 바로 포기한다)
fn client_error() -> Result<ProviderBooking, ProviderError> {
    Err(ProviderError::Client {
        status: 422,
        body: "{}".into(),
    })
}

fn call_record(booking_id: Option<BookingId>, outcome: &str, duration_ms: i32) -> NewProviderCall {
    NewProviderCall {
        provider_id: "alpha".into(),
        operation: "create_booking".into(),
        booking_id,
        job_id: None,
        attempt: 1,
        started_at: db::normalize_timestamp(Utc::now() - TimeDelta::seconds(1)),
        duration_ms,
        outcome: outcome.into(),
        http_status: None,
        rate_limit_remaining: None,
        retry_after_ms: None,
        error_detail: None,
        trace_id: None,
    }
}

/// mock 업체를 임의 포트에 띄우고 주소(`http://127.0.0.1:포트`)를 돌려준다
async fn start_mock() -> String {
    let state = mock_provider::AppState::new(&mock_provider::chaos::ChaosConfig::passthrough());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("임의 포트에 바인드할 수 있어야 한다");
    let addr = listener.local_addr().expect("주소를 알 수 있어야 한다");
    tokio::spawn(async move { axum::serve(listener, mock_provider::app(state)).await });
    format!("http://{addr}")
}
