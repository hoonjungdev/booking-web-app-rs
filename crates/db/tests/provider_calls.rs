//! 업체 호출 기록 저장소 테스트

mod common;

use chrono::{TimeDelta, Utc};
use db::{
    PgPool, bookings,
    provider_calls::{self, NewProviderCall},
};

fn call(booking_id: Option<domain::BookingId>, attempt: i32, outcome: &str) -> NewProviderCall {
    NewProviderCall {
        provider_id: "alpha".into(),
        operation: "create_booking".into(),
        booking_id,
        job_id: Some(1),
        attempt,
        // DB 정밀도(마이크로초)로 맞춰야 다시 읽은 값과 비교할 수 있다
        started_at: db::normalize_timestamp(Utc::now() - TimeDelta::seconds(1)),
        duration_ms: 120,
        outcome: outcome.into(),
        http_status: None,
        rate_limit_remaining: None,
        retry_after_ms: None,
        error_detail: None,
        trace_id: None,
    }
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 여러_건을_한_번에_넣고_예약별로_읽는다(pool: PgPool) {
    let client = common::create_client(&pool, "app").await;
    let new = common::new_booking(client, "key-1", b"hash-1");
    bookings::accept(&pool, &new).await.unwrap();

    let first = NewProviderCall {
        http_status: Some(503),
        error_detail: Some("업체 서버 오류 (HTTP 503)".into()),
        ..call(Some(new.id), 1, "server_error")
    };
    let second = NewProviderCall {
        http_status: Some(429),
        rate_limit_remaining: Some(0),
        retry_after_ms: Some(1000),
        trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".into()),
        ..call(Some(new.id), 2, "rate_limited")
    };
    // 예약과 상관없는 호출 (값이 비어 있는 칸은 NULL로 들어간다)
    let unrelated = NewProviderCall {
        job_id: None,
        ..call(None, 1, "success")
    };

    let inserted = provider_calls::insert_batch(&pool, &[first.clone(), second.clone(), unrelated])
        .await
        .unwrap();
    assert_eq!(inserted, 3);

    let saved = provider_calls::for_booking(&pool, new.id).await.unwrap();
    assert_eq!(saved.len(), 2);
    for (record, expected) in saved.iter().zip([&first, &second]) {
        assert_eq!(record.provider_id, expected.provider_id);
        assert_eq!(record.booking_id, expected.booking_id);
        assert_eq!(record.job_id, expected.job_id);
        assert_eq!(record.attempt, expected.attempt);
        assert_eq!(record.started_at, expected.started_at);
        assert_eq!(record.outcome, expected.outcome);
        assert_eq!(record.http_status, expected.http_status);
        assert_eq!(record.rate_limit_remaining, expected.rate_limit_remaining);
        assert_eq!(record.retry_after_ms, expected.retry_after_ms);
        assert_eq!(record.error_detail, expected.error_detail);
        assert_eq!(record.trace_id, expected.trace_id);
    }
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 빈_목록은_아무것도_하지_않는다(pool: PgPool) {
    assert_eq!(provider_calls::insert_batch(&pool, &[]).await.unwrap(), 0);
}

/// `at` 시각에 시작한 호출 한 건
fn call_at(
    at: chrono::DateTime<Utc>,
    attempt: i32,
    outcome: &str,
    duration_ms: i32,
) -> NewProviderCall {
    NewProviderCall {
        started_at: at,
        duration_ms,
        ..call(None, attempt, outcome)
    }
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 구간별로_지연_분위수와_결과별_수를_센다(pool: PgPool) {
    // 기준 시각을 1분 경계에 맞춰 두 구간(1분 간격)에 호출을 나눠 넣는다
    let start: chrono::DateTime<Utc> = "2026-11-10T03:00:00Z".parse().unwrap();
    let second_bucket = start + TimeDelta::minutes(1);
    let calls = vec![
        call_at(start + TimeDelta::seconds(1), 1, "success", 100),
        call_at(start + TimeDelta::seconds(2), 1, "success", 200),
        call_at(start + TimeDelta::seconds(3), 1, "timeout", 3000),
        call_at(start + TimeDelta::seconds(4), 2, "success", 300),
        // 관문에서 막힌 시도: 수에는 넣고 지연에서는 뺀다
        call_at(start + TimeDelta::seconds(5), 1, "circuit_open", 0),
        NewProviderCall {
            rate_limit_remaining: Some(7),
            ..call_at(second_bucket + TimeDelta::seconds(1), 1, "success", 50)
        },
        NewProviderCall {
            rate_limit_remaining: Some(3),
            ..call_at(second_bucket + TimeDelta::seconds(2), 1, "rate_limited", 10)
        },
        // 기간 밖
        call_at(start - TimeDelta::seconds(1), 1, "success", 999),
        NewProviderCall {
            provider_id: "beta".into(),
            ..call_at(start + TimeDelta::seconds(1), 1, "success", 999)
        },
    ];
    provider_calls::insert_batch(&pool, &calls).await.unwrap();

    let to = start + TimeDelta::minutes(2);
    let minute = std::time::Duration::from_secs(60);
    let buckets = provider_calls::timeseries(&pool, "alpha", start, to, minute)
        .await
        .unwrap();
    assert_eq!(buckets.len(), 2);

    let first = &buckets[0];
    assert_eq!(first.bucket, start);
    assert_eq!(first.calls, 5);
    assert_eq!(first.retries, 1);
    // 지연은 부른 시도 4건(100, 200, 300, 3000)으로 계산한다. 중앙값 = (200 + 300) / 2
    assert_eq!(first.p50_ms, Some(250.0));
    assert!(first.p99_ms.unwrap() > 2_900.0);
    assert_eq!(first.min_rate_limit_remaining, None);

    let second = &buckets[1];
    assert_eq!(second.bucket, second_bucket);
    assert_eq!(second.calls, 2);
    assert_eq!(second.min_rate_limit_remaining, Some(3));

    let outcomes = provider_calls::outcome_counts(&pool, "alpha", start, to, minute)
        .await
        .unwrap();
    let first_outcomes: Vec<_> = outcomes
        .iter()
        .filter(|o| o.bucket == start)
        .map(|o| (o.outcome.as_str(), o.count))
        .collect();
    assert_eq!(
        first_outcomes,
        vec![("circuit_open", 1), ("success", 3), ("timeout", 1)]
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 업체별_요약은_부른_시도와_막힌_시도를_나눠_센다(pool: PgPool) {
    let now = db::normalize_timestamp(Utc::now());
    let calls = vec![
        call_at(now - TimeDelta::seconds(30), 1, "success", 100),
        call_at(now - TimeDelta::seconds(29), 1, "rejected", 100),
        call_at(now - TimeDelta::seconds(28), 1, "rate_limited", 10),
        call_at(now - TimeDelta::seconds(27), 2, "server_error", 500),
        call_at(now - TimeDelta::seconds(26), 1, "circuit_open", 0),
        call_at(now - TimeDelta::seconds(25), 1, "throttled", 0),
        // 요약 기간(1시간) 밖
        call_at(now - TimeDelta::hours(2), 1, "success", 100),
    ];
    provider_calls::insert_batch(&pool, &calls).await.unwrap();

    let summary = provider_calls::summary(
        &pool,
        &["alpha".to_owned(), "beta".to_owned()],
        now - TimeDelta::hours(1),
    )
    .await
    .unwrap();
    assert_eq!(summary.len(), 1, "호출이 없는 업체는 빠진다");
    let alpha = &summary[0];
    assert_eq!(alpha.provider_id, "alpha");
    assert_eq!(alpha.attempts, 6);
    assert_eq!(alpha.called, 4);
    assert_eq!(
        alpha.answered, 2,
        "업무적 거절도 업체가 정상으로 답한 것이다"
    );
    assert_eq!(alpha.rate_limited, 1);
    assert_eq!(alpha.blocked, 2);
    assert_eq!(alpha.retries, 1);
    assert_eq!(alpha.p50_ms, Some(100.0));
}
