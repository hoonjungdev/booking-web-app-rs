//! 업체 ID → 어댑터

use std::{collections::HashMap, sync::Arc};

use db::PgPool;
use domain::ProviderId;
use providers::{BuildError, ProviderAdapter, ProviderPolicy};

/// 워커가 예약을 처리할 때 예약의 업체에 맞는 어댑터를 고르는 표.
///
/// 값이 `Arc<dyn ProviderAdapter>`라서 alpha · beta처럼 서로 다른 어댑터를 한 표에 담을 수 있다
/// (≈ C#의 `Dictionary<string, IProviderAdapter>`). 테스트에서는 가짜 어댑터를 넣는다.
#[derive(Default, Clone)]
pub struct ProviderRegistry {
    adapters: HashMap<ProviderId, Arc<dyn ProviderAdapter>>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    Db(#[from] db::DbError),
    #[error("업체 {provider}의 어댑터를 만들 수 없습니다: {source}")]
    Build {
        provider: ProviderId,
        source: BuildError,
    },
}

impl ProviderRegistry {
    /// `providers` 테이블의 사용 중인 업체로 어댑터를 만든다. 하나라도 실패하면 시작하지 않는다 (설정 오류를 일찍 드러냄).
    pub async fn load(pool: &PgPool) -> Result<Self, RegistryError> {
        let mut registry = Self::default();
        for record in db::providers::list_enabled(pool).await? {
            let adapter = ProviderPolicy::from_json(record.policy)
                .and_then(|policy| {
                    providers::build_adapter(&record.adapter_kind, &record.base_url, &policy)
                })
                .map_err(|source| RegistryError::Build {
                    provider: record.id.clone(),
                    source,
                })?;
            registry.insert(record.id, adapter);
        }
        Ok(registry)
    }

    pub fn insert(&mut self, id: ProviderId, adapter: Arc<dyn ProviderAdapter>) {
        self.adapters.insert(id, adapter);
    }

    /// `Arc`를 복제해 돌려준다. 복제는 참조 카운트만 늘리므로 비용이 거의 없다.
    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn ProviderAdapter>> {
        self.adapters.get(id).cloned()
    }

    pub fn ids(&self) -> Vec<&ProviderId> {
        let mut ids: Vec<_> = self.adapters.keys().collect();
        ids.sort();
        ids
    }
}
