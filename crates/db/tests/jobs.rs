//! 작업 큐 테스트: 가져가기, 재예약, 포기, lease 만료 후 회수, fencing

mod common;

use std::time::Duration;

use db::{PgPool, bookings, jobs};

/// 예약 하나를 접수해 `submit_booking` 작업 하나를 만든다
async fn one_job(pool: &PgPool) -> domain::BookingId {
    let client = common::create_client(pool, "app").await;
    let new = common::new_booking(client, "key-1", b"hash-1");
    bookings::accept(pool, &new)
        .await
        .expect("예약을 접수할 수 있어야 한다");
    new.id
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 작업은_한_번에_하나의_워커만_가져간다(pool: PgPool) {
    let booking_id = one_job(&pool).await;

    let job = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.booking_id, Some(booking_id));
    assert_eq!(job.attempts, 1);
    assert_eq!(job.max_attempts, jobs::DEFAULT_MAX_ATTEMPTS);

    // lease가 남아 있는 동안에는 아무도 다시 가져가지 못한다
    assert!(
        jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
            .await
            .unwrap()
            .is_none()
    );

    let mut conn = pool.acquire().await.unwrap();
    assert!(jobs::complete(&mut conn, job.lease()).await.unwrap());
    let record = &jobs::for_booking(&pool, booking_id).await.unwrap()[0];
    assert_eq!(record.status, "done");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 재예약한_작업은_시간이_되기_전에는_나오지_않는다(pool: PgPool) {
    let booking_id = one_job(&pool).await;
    let job = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let delay = Duration::from_secs(300);
    assert!(
        jobs::retry_later(&mut conn, job.lease(), delay, "timeout")
            .await
            .unwrap()
    );

    assert!(
        jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
            .await
            .unwrap()
            .is_none()
    );
    let record = &jobs::for_booking(&pool, booking_id).await.unwrap()[0];
    assert_eq!(record.status, "queued");
    assert_eq!(record.attempts, 1);
    assert_eq!(record.last_error.as_deref(), Some("timeout"));
    assert!(record.run_at > chrono::Utc::now() + chrono::TimeDelta::seconds(200));
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn lease가_만료되면_다른_워커가_회수하고_원래_워커의_처리는_무시된다(
    pool: PgPool,
) {
    one_job(&pool).await;

    // 워커 A가 가져갔지만 lease(0초)가 바로 만료된 상황 (처리 도중 죽었다고 가정)
    let first = jobs::claim_next(&pool, Duration::ZERO, &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    // 워커 B가 같은 작업을 회수한다. attempts가 늘어난다.
    let second = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(second.attempts, first.attempts + 1);

    // 늦게 깨어난 워커 A의 완료 처리는 반영되지 않는다 (fencing)
    let mut conn = pool.acquire().await.unwrap();
    assert!(!jobs::complete(&mut conn, first.lease()).await.unwrap());
    assert!(
        !jobs::mark_dead(&mut conn, first.lease(), "late")
            .await
            .unwrap()
    );
    // 워커 B의 처리는 반영된다
    assert!(jobs::complete(&mut conn, second.lease()).await.unwrap());
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 포기한_작업은_dead로_남는다(pool: PgPool) {
    let booking_id = one_job(&pool).await;
    let job = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();

    let mut conn = pool.acquire().await.unwrap();
    assert!(
        jobs::mark_dead(&mut conn, job.lease(), "attempts_exhausted")
            .await
            .unwrap()
    );

    let record = &jobs::for_booking(&pool, booking_id).await.unwrap()[0];
    assert_eq!(record.status, "dead");
    assert_eq!(record.last_error.as_deref(), Some("attempts_exhausted"));
    assert!(
        jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 미룬_작업은_시도_횟수를_되돌린다(pool: PgPool) {
    let booking_id = one_job(&pool).await;
    let job = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.attempts, 1);

    let mut conn = pool.acquire().await.unwrap();
    assert!(
        jobs::defer(&mut conn, job.lease(), Duration::ZERO, "circuit_open")
            .await
            .unwrap()
    );
    let record = &jobs::for_booking(&pool, booking_id).await.unwrap()[0];
    assert_eq!(record.status, "queued");
    assert_eq!(
        record.attempts, 0,
        "업체를 부르지 않았으므로 시도로 세지 않는다"
    );
    assert_eq!(record.last_error.as_deref(), Some("circuit_open"));

    // 다시 가져가면 다시 1번째 시도다.
    // (미룬 워커는 defer가 마지막 쓰기라서, 같은 시도 번호가 다시 나와도 그 워커가 이 작업을 건드릴 일은 없다)
    let again = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.attempts, 1);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 진행_상황을_payload에_남기고_다시_가져갈_때_읽는다(pool: PgPool) {
    one_job(&pool).await;
    let job = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.payload, serde_json::json!({}));

    let mut conn = pool.acquire().await.unwrap();
    let progress = serde_json::json!({ "not_found_checks": 1 });
    assert!(
        jobs::retry_later_with_payload(
            &mut conn,
            job.lease(),
            Duration::ZERO,
            "not_found",
            &progress
        )
        .await
        .unwrap()
    );

    let again = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.payload, progress);
    assert_eq!(again.attempts, 2);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 가져갈_때_실행_시각보다_얼마나_밀렸는지_알려_준다(pool: PgPool) {
    one_job(&pool).await;
    // 준비: 7초 전에 실행했어야 하는 작업으로 만든다
    sqlx::query("UPDATE jobs SET run_at = now() - interval '7 seconds'")
        .execute(&pool)
        .await
        .unwrap();

    let job = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert!(
        (Duration::from_secs(7)..Duration::from_secs(9)).contains(&job.lag),
        "밀린 시간: {:?}",
        job.lag
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 종류별로_대기_작업과_dead_작업을_센다(pool: PgPool) {
    assert_eq!(jobs::queue_depths(&pool).await.unwrap(), vec![]);

    // submit_booking 작업 3개: 하나는 바로 실행 가능, 하나는 나중에 실행, 하나는 dead
    let client = common::create_client(&pool, "app").await;
    for key in ["key-1", "key-2", "key-3"] {
        let new = common::new_booking(client, key, key.as_bytes());
        bookings::accept(&pool, &new).await.unwrap();
    }
    let mut conn = pool.acquire().await.unwrap();
    let later = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    jobs::retry_later(
        &mut conn,
        later.lease(),
        Duration::from_secs(300),
        "timeout",
    )
    .await
    .unwrap();
    let dead = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    jobs::mark_dead(&mut conn, dead.lease(), "attempts_exhausted")
        .await
        .unwrap();
    // 다른 종류의 작업 하나
    jobs::enqueue(
        &mut conn,
        jobs::JobKind::ResolveUnknown,
        None,
        jobs::DEFAULT_MAX_ATTEMPTS,
        None,
    )
    .await
    .unwrap();

    let mut depths = jobs::queue_depths(&pool).await.unwrap();
    depths.sort_by(|a, b| a.kind.cmp(&b.kind));
    assert_eq!(
        depths,
        vec![
            jobs::QueueDepth {
                kind: "resolve_unknown".into(),
                ready: 1,
                dead: 0
            },
            jobs::QueueDepth {
                kind: "submit_booking".into(),
                ready: 1,
                dead: 1
            },
        ],
        "나중에 실행할 작업(재예약)과 처리 중인 작업은 대기 수에 넣지 않는다"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 작업_목록은_상태와_종류로_거르고_커서로_나눠_읽는다(pool: PgPool) {
    let first = one_job(&pool).await;
    let client = common::create_client(&pool, "other").await;
    let second = common::new_booking(client, "key-2", b"hash-2");
    bookings::accept(&pool, &second).await.unwrap();

    // 첫 예약의 작업을 dead로 만든다
    let job = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.booking_id, Some(first));
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        jobs::mark_dead(&mut conn, job.lease(), "boom")
            .await
            .unwrap()
    );

    let all = jobs::list(&pool, &jobs::JobFilter::default(), 10)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    assert!(all[0].id > all[1].id, "최근에 등록한 것부터");

    let dead = jobs::JobFilter {
        status: Some("dead".into()),
        ..Default::default()
    };
    let dead = jobs::list(&pool, &dead, 10).await.unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].id, job.id);
    assert_eq!(dead[0].last_error.as_deref(), Some("boom"));

    let other_kind = jobs::JobFilter {
        kind: Some("resolve_unknown".into()),
        ..Default::default()
    };
    assert!(jobs::list(&pool, &other_kind, 10).await.unwrap().is_empty());

    let after_newest = jobs::JobFilter {
        before: Some(all[0].id),
        ..Default::default()
    };
    let rest = jobs::list(&pool, &after_newest, 10).await.unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].id, all[1].id);

    let fetched = jobs::get(&mut conn, job.id).await.unwrap().unwrap();
    assert_eq!(fetched.status, "dead");
    assert!(jobs::get(&mut conn, -1).await.unwrap().is_none());
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 종류별로_대기_미룸_실행_dead를_센다(pool: PgPool) {
    for n in 0..4 {
        let client = common::create_client(&pool, &format!("app-{n}")).await;
        let new = common::new_booking(client, "key", b"hash");
        bookings::accept(&pool, &new).await.unwrap();
    }
    let mut conn = pool.acquire().await.unwrap();
    // 작업 4개: 실행 중 1, 미룸 1, dead 1, 대기 1
    let running = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    let delayed = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert!(
        jobs::retry_later(
            &mut conn,
            delayed.lease(),
            Duration::from_secs(300),
            "later"
        )
        .await
        .unwrap()
    );
    let dead = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert!(
        jobs::mark_dead(&mut conn, dead.lease(), "boom")
            .await
            .unwrap()
    );
    assert_ne!(running.id, dead.id);

    let counts = jobs::counts(&pool).await.unwrap();
    assert_eq!(
        counts,
        vec![jobs::KindCounts {
            kind: "submit_booking".into(),
            ready: 1,
            scheduled: 1,
            running: 1,
            dead: 1,
        }]
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn dead_작업은_같은_행으로_되살리고_시도_한도를_늘린다(pool: PgPool) {
    let booking_id = one_job(&pool).await;
    let job = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();

    // dead가 아닌 작업은 되살리지 않는다
    let payload = serde_json::json!({ "lookup_first": true });
    assert!(
        !jobs::requeue(&mut conn, job.id, "submit_booking", &payload, None)
            .await
            .unwrap()
    );
    assert!(
        jobs::has_active_for_booking(&mut conn, booking_id)
            .await
            .unwrap()
    );

    assert!(
        jobs::mark_dead(&mut conn, job.lease(), "boom")
            .await
            .unwrap()
    );
    assert!(
        !jobs::has_active_for_booking(&mut conn, booking_id)
            .await
            .unwrap()
    );
    assert_eq!(
        jobs::latest_dead_for_booking(&mut conn, booking_id)
            .await
            .unwrap(),
        Some(job.id)
    );

    let trace = serde_json::json!({ "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01" });
    assert!(
        jobs::requeue(&mut conn, job.id, "submit_booking", &payload, Some(&trace))
            .await
            .unwrap()
    );
    assert!(
        !jobs::requeue(&mut conn, job.id, "submit_booking", &payload, None)
            .await
            .unwrap(),
        "이미 되살린 작업을 다시 되살리지 않는다"
    );

    let again = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .expect("되살린 작업은 바로 실행할 수 있다");
    assert_eq!(again.id, job.id);
    assert_eq!(
        again.attempts,
        job.attempts + 1,
        "시도 횟수는 lease 증표라 되돌리지 않고 이어서 센다"
    );
    assert_eq!(
        again.max_attempts,
        job.attempts + jobs::DEFAULT_MAX_ATTEMPTS,
        "되살린 시점부터 시도 한도를 다시 준다"
    );
    assert_eq!(again.payload, payload);
    assert_eq!(again.trace_context, Some(trace));
    assert_eq!(jobs::for_booking(&pool, booking_id).await.unwrap().len(), 1);

    // 되살리기 전의 lease를 쥔 채 멈춰 있던 워커가 깨어나도 새로 가져간 작업에 손대지 못한다 (fencing)
    assert!(!jobs::complete(&mut conn, job.lease()).await.unwrap());
    assert!(jobs::complete(&mut conn, again.lease()).await.unwrap());
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 처음부터_진행_상황을_정해_작업을_등록할_수_있다(pool: PgPool) {
    let booking_id = one_job(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let payload = serde_json::json!({ "lookup_first": true });
    let id = jobs::enqueue_with_payload(
        &mut conn,
        jobs::JobKind::SubmitBooking,
        Some(booking_id),
        jobs::DEFAULT_MAX_ATTEMPTS,
        &payload,
        None,
    )
    .await
    .unwrap();
    let saved = jobs::get(&mut conn, id).await.unwrap().unwrap();
    assert_eq!(saved.payload, payload);
    assert_eq!(saved.status, "queued");
}

// ───────────────────────── Phase 6: 업체별 가져오기, 보냄 표시, 대사 작업 ─────────────────────────

/// 업체 `beta`의 예약 하나를 접수한다 (beta는 마이그레이션이 넣어 둔 업체다)
async fn one_beta_job(pool: &PgPool) -> domain::BookingId {
    let client = common::create_client(pool, "beta-app").await;
    let mut new = common::new_booking(client, "beta-key", b"hash-beta");
    new.request.provider = provider("beta");
    bookings::accept(pool, &new)
        .await
        .expect("예약을 접수할 수 있어야 한다");
    new.id
}

fn provider(id: &str) -> domain::ProviderId {
    domain::ProviderId::parse(id).expect("올바른 업체 ID")
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 작업의_업체는_예약에서_가져온다(pool: PgPool) {
    let alpha = one_job(&pool).await;
    let beta = one_beta_job(&pool).await;
    assert_eq!(
        jobs::for_booking(&pool, alpha).await.unwrap()[0]
            .provider_id
            .as_deref(),
        Some("alpha")
    );
    assert_eq!(
        jobs::for_booking(&pool, beta).await.unwrap()[0]
            .provider_id
            .as_deref(),
        Some("beta")
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 업체별_루프는_자기_업체의_작업만_가져간다(pool: PgPool) {
    let alpha = one_job(&pool).await;
    let beta = one_beta_job(&pool).await;
    // 업체가 없는 작업 (다른 버전이 등록한 작업 등)
    sqlx::query("INSERT INTO jobs (kind, max_attempts) VALUES ('mystery_kind', 1)")
        .execute(&pool)
        .await
        .unwrap();
    let lease = Duration::from_secs(60);

    let beta_job = jobs::claim_next(&pool, lease, &jobs::ClaimScope::Provider(provider("beta")))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(beta_job.booking_id, Some(beta));
    assert_eq!(beta_job.provider_id.as_deref(), Some("beta"));
    assert!(
        jobs::claim_next(&pool, lease, &jobs::ClaimScope::Provider(provider("beta")))
            .await
            .unwrap()
            .is_none(),
        "beta 작업은 하나뿐이다"
    );

    // alpha와 beta에는 전용 루프가 있다고 할 때, 남은 루프는 업체가 없는 작업만 가져간다
    let others = jobs::ClaimScope::Unassigned(vec![provider("alpha"), provider("beta")]);
    let unassigned = jobs::claim_next(&pool, lease, &others)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unassigned.kind, "mystery_kind");
    assert!(
        jobs::claim_next(&pool, lease, &others)
            .await
            .unwrap()
            .is_none()
    );

    // alpha 전용 루프가 없는 프로세스라면 그 루프가 alpha 작업도 맡는다 (영영 남지 않게)
    let only_beta = jobs::ClaimScope::Unassigned(vec![provider("beta")]);
    let alpha_job = jobs::claim_next(&pool, lease, &only_beta)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(alpha_job.booking_id, Some(alpha));
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 보냄_표시는_lease가_있어야_남고_결과를_기록하면_지워진다(
    pool: PgPool,
) {
    let booking_id = one_job(&pool).await;
    let job = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.sent_at, None);
    let mut conn = pool.acquire().await.unwrap();

    // lease가 다르면 표시하지 못한다 (보내면 안 된다)
    let stale = jobs::Lease {
        job_id: job.id,
        attempt: job.attempts + 1,
    };
    let renew = Duration::from_secs(600);
    assert!(!jobs::mark_sent(&mut conn, stale, renew).await.unwrap());
    assert!(
        jobs::mark_sent(&mut conn, job.lease(), renew)
            .await
            .unwrap()
    );
    let record = &jobs::for_booking(&pool, booking_id).await.unwrap()[0];
    assert!(record.sent_at.is_some());
    // 보내기 직전에 lease를 새로 잡는다 (60초로 가져갔지만 지금부터 10분)
    let (renewed,): (bool,) = sqlx::query_as(
        "SELECT locked_until > now() + interval '9 minutes' FROM jobs WHERE id = $1",
    )
    .bind(job.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(renewed, "제출 한 번이 끝날 시간이 늘 남는다");

    // 결과를 기록하면(여기서는 재예약) 표시가 지워진다
    assert!(
        jobs::retry_later(&mut conn, job.lease(), Duration::ZERO, "connect_error")
            .await
            .unwrap()
    );
    let again = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.sent_at, None);

    // 표시한 뒤 결과를 남기지 못하고 lease가 끝나면, 다시 가져간 쪽이 표시를 본다
    assert!(
        jobs::mark_sent(&mut conn, again.lease(), renew)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE jobs SET locked_until = now() - interval '1 second' WHERE id = $1")
        .bind(again.id)
        .execute(&pool)
        .await
        .unwrap();
    let recovered = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.id, again.id);
    assert!(
        recovered.sent_at.is_some(),
        "앞선 워커가 보낸 뒤 멈췄다는 표시"
    );

    // 미루기도 표시를 지운다
    assert!(
        jobs::defer(&mut conn, recovered.lease(), Duration::ZERO, "circuit_open")
            .await
            .unwrap()
    );
    let record = &jobs::for_booking(&pool, booking_id).await.unwrap()[0];
    assert_eq!(record.sent_at, None);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 대사_작업은_업체마다_하나만_대기한다(pool: PgPool) {
    let alpha = provider("alpha");
    let mut conn = pool.acquire().await.unwrap();
    let first = jobs::schedule_reconcile(&mut conn, &alpha, Duration::from_secs(600), None)
        .await
        .unwrap();
    assert!(first.is_some());
    assert_eq!(
        jobs::schedule_reconcile(&mut conn, &alpha, Duration::ZERO, None)
            .await
            .unwrap(),
        None,
        "이미 대기 중이면 새로 넣지 않는다"
    );
    // 다른 업체는 따로 센다
    assert!(
        jobs::schedule_reconcile(&mut conn, &provider("beta"), Duration::ZERO, None)
            .await
            .unwrap()
            .is_some()
    );

    // "지금 대사"는 대기 중인 작업의 실행 시각을 당긴다
    let now = jobs::run_reconcile_now(&mut conn, &alpha, None)
        .await
        .unwrap();
    assert_eq!(Some(now.id), first);
    assert_eq!(now.kind, "reconcile_provider");
    assert!(now.run_at <= chrono::Utc::now());

    // 실행 중이면 그 작업을 그대로 돌려준다
    let claimed = jobs::claim_next(
        &pool,
        Duration::from_secs(60),
        &jobs::ClaimScope::Provider(alpha.clone()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(Some(claimed.id), first);
    let running = jobs::run_reconcile_now(&mut conn, &alpha, None)
        .await
        .unwrap();
    assert_eq!(running.id, claimed.id);
    assert_eq!(running.status, "running");

    // 끝나면 다음 실행을 새로 등록할 수 있다 (작업 체인)
    assert!(jobs::complete(&mut conn, claimed.lease()).await.unwrap());
    assert!(
        jobs::schedule_reconcile(&mut conn, &alpha, Duration::from_secs(600), None)
            .await
            .unwrap()
            .is_some()
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 예약의_dead_작업을_끝난_것으로_정리한다(pool: PgPool) {
    let booking_id = one_job(&pool).await;
    let job = jobs::claim_next(&pool, Duration::from_secs(60), &jobs::ClaimScope::Any)
        .await
        .unwrap()
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    jobs::mark_dead(&mut conn, job.lease(), "deadline_exceeded")
        .await
        .unwrap();

    let closed =
        jobs::close_dead_for_booking(&mut conn, booking_id, "대사가 업체에서 확정된 예약을 찾음")
            .await
            .unwrap();
    assert_eq!(closed, 1);
    let record = &jobs::for_booking(&pool, booking_id).await.unwrap()[0];
    assert_eq!(record.status, "done");
    assert_eq!(
        record.last_error.as_deref(),
        Some("대사가 업체에서 확정된 예약을 찾음")
    );
}
