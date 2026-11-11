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
