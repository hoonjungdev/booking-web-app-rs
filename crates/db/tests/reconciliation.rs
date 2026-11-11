//! 대사 이슈 테스트: 미해결 이슈는 하나만, 자동 처리한 이슈는 처리 완료로, 목록 · 처리 완료 · 개수

mod common;

use db::{
    PgPool, bookings,
    reconciliation::{self, IssueFilter, IssueKind, NewIssue},
};
use domain::{BookingId, ProviderId};
use serde_json::json;
use uuid::Uuid;

fn issue(kind: IssueKind, booking_id: Option<BookingId>) -> NewIssue {
    NewIssue {
        provider_id: ProviderId::parse("alpha").expect("올바른 업체 ID"),
        booking_id,
        provider_booking_id: Some("rsv_1".into()),
        kind,
        detail: json!({ "ours": "confirmed" }),
        resolved_by: None,
    }
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 같은_미해결_이슈는_다시_쌓지_않는다(pool: PgPool) {
    let booking = Some(BookingId::from_uuid(Uuid::now_v7()));
    let mut conn = pool.acquire().await.unwrap();

    let first = reconciliation::record(&mut conn, &issue(IssueKind::MissingAtProvider, booking))
        .await
        .unwrap();
    assert!(first.is_some());
    // 10분 뒤 대사가 같은 불일치를 또 찾았다
    let again = reconciliation::record(&mut conn, &issue(IssueKind::MissingAtProvider, booking))
        .await
        .unwrap();
    assert_eq!(again, None);
    // 업체 예약 ID가 없는 이슈끼리도 겹치지 않는다 (NULLS NOT DISTINCT)
    let no_provider_id = NewIssue {
        provider_booking_id: None,
        ..issue(IssueKind::DuplicateSuspected, booking)
    };
    assert!(
        reconciliation::record(&mut conn, &no_provider_id)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        reconciliation::record(&mut conn, &no_provider_id)
            .await
            .unwrap(),
        None
    );

    // 처리 완료로 표시한 뒤 다시 생기면 새 이슈다
    let id = first.unwrap();
    assert!(
        reconciliation::resolve(&mut conn, id, "admin:kim")
            .await
            .unwrap()
    );
    assert!(
        !reconciliation::resolve(&mut conn, id, "admin:lee")
            .await
            .unwrap(),
        "이미 처리 완료"
    );
    let resolved = reconciliation::get(&mut conn, id).await.unwrap().unwrap();
    assert_eq!(resolved.resolved_by.as_deref(), Some("admin:kim"));
    assert!(
        reconciliation::record(&mut conn, &issue(IssueKind::MissingAtProvider, booking))
            .await
            .unwrap()
            .is_some()
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 자동으로_바로잡은_이슈는_처리_완료로_기록한다(pool: PgPool) {
    let booking = Some(BookingId::from_uuid(Uuid::now_v7()));
    let mut conn = pool.acquire().await.unwrap();
    let fixed = NewIssue {
        resolved_by: Some("reconciler".into()),
        ..issue(IssueKind::StatusMismatch, booking)
    };
    let id = reconciliation::record(&mut conn, &fixed)
        .await
        .unwrap()
        .unwrap();
    let stored = reconciliation::get(&mut conn, id).await.unwrap().unwrap();
    assert!(stored.resolved_at.is_some());
    assert_eq!(stored.resolved_by.as_deref(), Some("reconciler"));
    assert_eq!(stored.detail, json!({ "ours": "confirmed" }));
    assert!(reconciliation::open_counts(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 목록은_해결_여부_업체_종류로_거르고_개수를_센다(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    for kind in [
        IssueKind::MissingAtProvider,
        IssueKind::DuplicateSuspected,
        IssueKind::StatusMismatch,
    ] {
        reconciliation::record(
            &mut conn,
            &issue(kind, Some(BookingId::from_uuid(Uuid::now_v7()))),
        )
        .await
        .unwrap();
    }
    let beta = NewIssue {
        provider_id: ProviderId::parse("beta").unwrap(),
        ..issue(IssueKind::DuplicateSuspected, None)
    };
    let beta_id = reconciliation::record(&mut conn, &beta)
        .await
        .unwrap()
        .unwrap();
    reconciliation::resolve(&mut conn, beta_id, "admin:kim")
        .await
        .unwrap();

    let open = IssueFilter {
        resolved: Some(false),
        ..IssueFilter::default()
    };
    let issues = reconciliation::list(&pool, &open, 10).await.unwrap();
    assert_eq!(issues.len(), 3);
    assert!(issues.windows(2).all(|w| w[0].id > w[1].id), "최근 것부터");

    let duplicates = IssueFilter {
        kind: Some(IssueKind::DuplicateSuspected),
        ..IssueFilter::default()
    };
    assert_eq!(
        reconciliation::list(&pool, &duplicates, 10)
            .await
            .unwrap()
            .len(),
        2
    );
    let beta_only = IssueFilter {
        provider: Some(ProviderId::parse("beta").unwrap()),
        resolved: Some(true),
        ..IssueFilter::default()
    };
    let resolved = reconciliation::list(&pool, &beta_only, 10).await.unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].id, beta_id);
    let page = reconciliation::list(
        &pool,
        &IssueFilter {
            before: Some(issues[0].id),
            ..open
        },
        10,
    )
    .await
    .unwrap();
    assert_eq!(page.len(), 2, "커서 다음부터");

    let counts = reconciliation::open_counts(&pool).await.unwrap();
    assert_eq!(
        counts,
        vec![
            (IssueKind::DuplicateSuspected, 1),
            (IssueKind::MissingAtProvider, 1),
            (IssueKind::StatusMismatch, 1),
        ]
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 대사는_업체와_슬롯_기간으로_예약을_고른다(pool: PgPool) {
    let client = common::create_client(&pool, "app").await;
    let new = common::new_booking(client, "k1", b"h1");
    bookings::accept(&pool, &new).await.unwrap();
    let slot = new.request.slot_start;
    let alpha = ProviderId::parse("alpha").unwrap();

    let around = bookings::for_reconciliation(
        &pool,
        &alpha,
        slot - chrono::TimeDelta::hours(1),
        slot + chrono::TimeDelta::hours(1),
    )
    .await
    .unwrap();
    assert_eq!(around.len(), 1);
    assert_eq!(around[0].id, new.id);
    // to는 포함하지 않는다
    assert!(
        bookings::for_reconciliation(&pool, &alpha, slot - chrono::TimeDelta::hours(1), slot)
            .await
            .unwrap()
            .is_empty()
    );
    let beta = ProviderId::parse("beta").unwrap();
    assert!(
        bookings::for_reconciliation(
            &pool,
            &beta,
            slot - chrono::TimeDelta::hours(1),
            slot + chrono::TimeDelta::hours(1)
        )
        .await
        .unwrap()
        .is_empty()
    );
}
