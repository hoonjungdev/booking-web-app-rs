//! 업체 ID → 장애 대응 파이프라인으로 감싼 업체

use std::{collections::HashMap, sync::Arc, time::Duration};

use db::PgPool;
use domain::ProviderId;
use providers::{BuildError, CallRecorder, ProviderPolicy, ResilientProvider};

/// 워커가 예약을 처리할 때 예약의 업체에 맞는 [`ResilientProvider`]를 고르는 표.
///
/// 업체마다 서킷 · rate limiter · bulkhead 상태를 하나씩 가져야 하므로, 업체별로 한 번 만들어 `Arc`로 공유한다
/// (≈ C#에서 업체별로 이름 붙인 `HttpClient` + Polly 정책을 싱글턴으로 등록하는 것). 테스트에서는 가짜 어댑터를 감싸 넣는다.
#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<ProviderId, Arc<ResilientProvider>>,
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
    /// `providers` 테이블의 사용 중인 업체로 파이프라인을 만든다. 하나라도 실패하면 시작하지 않는다 (설정 오류를 일찍 드러냄).
    ///
    /// `recorder`는 업체마다 복제해 나눠 준다.
    pub async fn load(pool: &PgPool, recorder: CallRecorder) -> Result<Self, RegistryError> {
        let mut registry = Self::default();
        for record in db::providers::list_enabled(pool).await? {
            let built = ProviderPolicy::from_json(record.policy).and_then(|policy| {
                let adapter =
                    providers::build_adapter(&record.adapter_kind, &record.base_url, &policy)?;
                Ok((adapter, policy))
            });
            let (adapter, policy) = built.map_err(|source| RegistryError::Build {
                provider: record.id.clone(),
                source,
            })?;
            registry.insert(Arc::new(ResilientProvider::new(
                record.id,
                adapter,
                policy,
                recorder.clone(),
            )));
        }
        Ok(registry)
    }

    pub fn insert(&mut self, provider: Arc<ResilientProvider>) {
        self.providers.insert(provider.id().clone(), provider);
    }

    /// `Arc`를 복제해 돌려준다. 복제는 참조 카운트만 늘리므로 비용이 거의 없다.
    pub fn get(&self, id: &ProviderId) -> Option<Arc<ResilientProvider>> {
        self.providers.get(id).cloned()
    }

    /// 등록된 업체 중 호출 한 번이 가장 오래 걸릴 수 있는 시간 (업체가 없으면 0)
    pub fn longest_call(&self) -> Duration {
        self.providers
            .values()
            .map(|provider| provider.longest_call())
            .max()
            .unwrap_or(Duration::ZERO)
    }

    pub fn ids(&self) -> Vec<&ProviderId> {
        let mut ids: Vec<_> = self.providers.keys().collect();
        ids.sort();
        ids
    }
}
