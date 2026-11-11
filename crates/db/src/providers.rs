//! 외부 업체 설정 (`providers`)

use domain::ProviderId;

use crate::{DbError, PgPool};

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRecord {
    pub id: ProviderId,
    pub display_name: String,
    /// 어떤 어댑터 코드로 호출할지 (`alpha_v1` …)
    pub adapter_kind: String,
    pub base_url: String,
    pub enabled: bool,
    /// 업체별 정책 (타임아웃 등). 형식은 providers 크레이트의 `ProviderPolicy`
    pub policy: serde_json::Value,
}

#[tracing::instrument(name = "db.providers.find", skip_all)]
pub async fn find(pool: &PgPool, id: &ProviderId) -> Result<Option<ProviderRecord>, DbError> {
    let row = sqlx::query!(
        "SELECT id, display_name, adapter_kind, base_url, enabled, policy FROM providers WHERE id = $1",
        id.as_str(),
    )
    .fetch_optional(pool)
    .await?;

    // `transpose`: Option<Result<T>>를 Result<Option<T>>로 뒤집는다. 그래야 `?`로 에러만 꺼낼 수 있다.
    row.map(|r| {
        Ok(ProviderRecord {
            id: ProviderId::parse(&r.id).map_err(DbError::corrupt)?,
            display_name: r.display_name,
            adapter_kind: r.adapter_kind,
            base_url: r.base_url,
            enabled: r.enabled,
            policy: r.policy,
        })
    })
    .transpose()
}

pub async fn list_enabled(pool: &PgPool) -> Result<Vec<ProviderRecord>, DbError> {
    let rows = sqlx::query!(
        "SELECT id, display_name, adapter_kind, base_url, enabled, policy
         FROM providers WHERE enabled ORDER BY id",
    )
    .fetch_all(pool)
    .await?;

    // 반복자의 각 항목이 Result일 때 `collect::<Result<Vec<_>, _>>()`는 하나라도 Err면 그 Err를 돌려준다.
    rows.into_iter()
        .map(|r| {
            Ok(ProviderRecord {
                id: ProviderId::parse(&r.id).map_err(DbError::corrupt)?,
                display_name: r.display_name,
                adapter_kind: r.adapter_kind,
                base_url: r.base_url,
                enabled: r.enabled,
                policy: r.policy,
            })
        })
        .collect()
}
