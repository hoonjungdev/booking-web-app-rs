//! 관리자 계정 · 세션 · 감사 로그 테스트

use chrono::{TimeDelta, Utc};
use db::{
    PgPool,
    admin::{self, NewAuditEntry},
};
use serde_json::json;

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 계정을_만들고_이름으로_찾는다(pool: PgPool) {
    let user = admin::create_user(&pool, "kim", "$argon2id$hash", "operator")
        .await
        .unwrap();
    let found = admin::find_user(&pool, "kim").await.unwrap().unwrap();
    assert_eq!(found, user);
    assert_eq!(found.role, "operator");
    assert!(admin::find_user(&pool, "lee").await.unwrap().is_none());

    assert!(
        admin::create_user(&pool, "kim", "x", "viewer")
            .await
            .is_err(),
        "같은 이름은 만들 수 없다"
    );
    assert!(
        admin::create_user(&pool, "park", "x", "superuser")
            .await
            .is_err(),
        "정해진 역할만"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 세션은_만료되기_전까지만_찾을_수_있다(pool: PgPool) {
    let user = admin::create_user(&pool, "kim", "x", "admin")
        .await
        .unwrap();
    admin::create_session(
        &pool,
        "hash-live",
        user.id,
        Utc::now() + TimeDelta::hours(1),
    )
    .await
    .unwrap();
    admin::create_session(
        &pool,
        "hash-old",
        user.id,
        Utc::now() - TimeDelta::seconds(1),
    )
    .await
    .unwrap();

    let session = admin::find_session(&pool, "hash-live")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (session.username.as_str(), session.role.as_str()),
        ("kim", "admin")
    );
    assert!(
        admin::find_session(&pool, "hash-old")
            .await
            .unwrap()
            .is_none(),
        "만료된 세션"
    );
    assert!(admin::find_session(&pool, "nope").await.unwrap().is_none());

    // 다음 로그인 때 만료된 세션은 지워진다
    admin::create_session(&pool, "hash-new", user.id, Utc::now() + TimeDelta::hours(1))
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_sessions WHERE user_id = $1")
        .bind(user.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 2);

    admin::delete_session(&pool, "hash-live").await.unwrap();
    assert!(
        admin::find_session(&pool, "hash-live")
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn 감사_로그는_최근_것부터_커서로_읽는다(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    for action in [
        "booking.retry",
        "provider.circuit",
        "provider.policy.update",
    ] {
        admin::audit(
            &mut conn,
            &NewAuditEntry {
                actor: "admin:kim",
                action,
                target: Some("alpha"),
                detail: json!({ "n": 1 }),
                trace_id: None,
            },
        )
        .await
        .unwrap();
    }
    let page = admin::audit_log(&pool, None, 2).await.unwrap();
    let actions: Vec<_> = page.iter().map(|e| e.action.as_str()).collect();
    assert_eq!(actions, vec!["provider.policy.update", "provider.circuit"]);
    let next = admin::audit_log(&pool, Some(page[1].id), 2).await.unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].action, "booking.retry");
    assert_eq!(next[0].actor, "admin:kim");
}
