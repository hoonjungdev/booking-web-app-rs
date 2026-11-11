//! 외부 업체 설정 (`providers`)

use chrono::{DateTime, Utc};
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
    /// 설정을 마지막으로 바꾼 시각
    pub updated_at: DateTime<Utc>,
}

/// DB 행 → [`ProviderRecord`]. 업체 ID가 규칙에 맞지 않으면 데이터가 깨진 것으로 본다.
struct ProviderRow {
    id: String,
    display_name: String,
    adapter_kind: String,
    base_url: String,
    enabled: bool,
    policy: serde_json::Value,
    updated_at: DateTime<Utc>,
}

impl TryFrom<ProviderRow> for ProviderRecord {
    type Error = DbError;

    fn try_from(row: ProviderRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: ProviderId::parse(&row.id).map_err(DbError::corrupt)?,
            display_name: row.display_name,
            adapter_kind: row.adapter_kind,
            base_url: row.base_url,
            enabled: row.enabled,
            policy: row.policy,
            updated_at: row.updated_at,
        })
    }
}

#[tracing::instrument(name = "db.providers.find", skip_all)]
pub async fn find(pool: &PgPool, id: &ProviderId) -> Result<Option<ProviderRecord>, DbError> {
    let row = sqlx::query_as!(
        ProviderRow,
        "SELECT id, display_name, adapter_kind, base_url, enabled, policy, updated_at
         FROM providers WHERE id = $1",
        id.as_str(),
    )
    .fetch_optional(pool)
    .await?;

    // `transpose`: Option<Result<T>>를 Result<Option<T>>로 뒤집는다. 그래야 `?`로 에러만 꺼낼 수 있다.
    row.map(ProviderRecord::try_from).transpose()
}

/// 사용 중인 업체 (워커가 업체 파이프라인을 만들 때)
pub async fn list_enabled(pool: &PgPool) -> Result<Vec<ProviderRecord>, DbError> {
    let rows = sqlx::query_as!(
        ProviderRow,
        "SELECT id, display_name, adapter_kind, base_url, enabled, policy, updated_at
         FROM providers WHERE enabled ORDER BY id",
    )
    .fetch_all(pool)
    .await?;

    // 반복자의 각 항목이 Result일 때 `collect::<Result<Vec<_>, _>>()`는 하나라도 Err면 그 Err를 돌려준다.
    rows.into_iter().map(ProviderRecord::try_from).collect()
}

/// 사용하지 않는 업체까지 모두 (관리자 화면)
#[tracing::instrument(name = "db.providers.list_all", skip_all)]
pub async fn list_all(pool: &PgPool) -> Result<Vec<ProviderRecord>, DbError> {
    let rows = sqlx::query_as!(
        ProviderRow,
        "SELECT id, display_name, adapter_kind, base_url, enabled, policy, updated_at
         FROM providers ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(ProviderRecord::try_from).collect()
}
