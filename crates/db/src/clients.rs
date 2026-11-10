//! 예약 API 클라이언트 (`api_clients`)

use domain::ClientId;

use crate::{DbError, PgPool};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiClient {
    pub id: ClientId,
    pub name: String,
}

/// 클라이언트를 만든다. API 키는 호출하는 쪽에서 해시해 넘긴다 (원문은 저장하지 않는다).
pub async fn create(
    pool: &PgPool,
    id: ClientId,
    name: &str,
    api_key_hash: &str,
) -> Result<ApiClient, DbError> {
    sqlx::query!(
        "INSERT INTO api_clients (id, name, api_key_hash) VALUES ($1, $2, $3)",
        id.as_uuid(),
        name,
        api_key_hash,
    )
    .execute(pool)
    .await?;
    Ok(ApiClient {
        id,
        name: name.to_owned(),
    })
}

/// API 키 해시로 클라이언트를 찾는다
pub async fn find_by_key_hash(
    pool: &PgPool,
    api_key_hash: &str,
) -> Result<Option<ApiClient>, DbError> {
    let row = sqlx::query!(
        "SELECT id, name FROM api_clients WHERE api_key_hash = $1",
        api_key_hash,
    )
    .fetch_optional(pool)
    .await?;
    // `Option::map`: 값이 있을 때만 바꾸고, 없으면 None 그대로 (≈ C#의 `row?.Let(...)`)
    Ok(row.map(|r| ApiClient {
        id: ClientId::from_uuid(r.id),
        name: r.name,
    }))
}
