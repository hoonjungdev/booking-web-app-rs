//! 빈 시간 조회 캐시 (docs/04-api.md §2, docs/decisions.md D-37)
//!
//! ```text
//! GET /v1/providers/{provider}/availability ─▶ 캐시에 TTL(정책, 기본 30초) 안의 값이 있다 ─▶ 그대로 (stale: false)
//!                                          └▶ 없거나 오래됐다 ─▶ 업체 호출 ─┬─ 성공 ─▶ 캐시에 넣고 돌려준다
//!                                                                         └─ 실패 ─▶ 5분 안의 캐시 값이 있으면 그것을 (stale: true)
//!                                                                                    없으면 오류 (503)
//! ```
//! 업체가 잠깐 아파도 사용자는 조금 전의 빈 시간을 볼 수 있다 (stale-if-error). 오래된 값이라는 것은 `stale`로 알린다.
//! 업무적 거절(없는 자원 등)은 업체가 정상으로 답한 것이라 캐시로 덮지 않고 그대로 돌려준다.
//!
//! 캐시는 moka(≈ .NET의 `IMemoryCache`)를 쓴다. 항목은 최대 5분(stale-if-error 한도)까지만 두고, 그 안에서
//! "새것으로 볼 시간"(TTL)은 업체 정책(`availability_cache_ttl_ms`)으로 정한다. 정책이 바뀌면 다음 조회부터 새 TTL을 쓴다.
//!
//! 알아 둘 한계: 같은 칸을 동시에 처음 조회하면 업체를 여러 번 부를 수 있다 (요청 합치기를 하지 않는다). 업체 호출은
//! rate limiter · bulkhead를 거치므로 업체를 몰아치지는 않는다.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, NaiveDate, Utc};
use domain::ProviderId;
use moka::sync::Cache;
use providers::{AvailableSlot, ProviderError, ResilientProvider};

/// 업체 호출이 실패했을 때 대신 줄 수 있는 캐시 값의 최대 나이
pub const STALE_IF_ERROR: Duration = Duration::from_secs(5 * 60);
/// 캐시에 둘 최대 항목 수 (업체 · 자원 · 날짜 조합). 넘으면 오래 안 쓴 것부터 버린다.
const MAX_ENTRIES: u64 = 10_000;

/// 캐시 키: 업체 · 자원 · 날짜
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    provider: ProviderId,
    resource_id: String,
    date: NaiveDate,
}

#[derive(Debug, Clone)]
struct Entry {
    /// `Arc`: 캐시와 응답이 같은 목록을 복사 없이 나눠 쓴다
    slots: Arc<Vec<AvailableSlot>>,
    /// 업체에서 가져온 시각 (응답의 `fetched_at`)
    fetched_at: DateTime<Utc>,
    /// 같은 시각의 단조 시계 값 (나이 계산용. 시스템 시각이 바뀌어도 뒤로 가지 않는다)
    fetched: Instant,
}

/// 빈 시간 조회 결과
#[derive(Debug, Clone)]
pub struct Availability {
    pub slots: Arc<Vec<AvailableSlot>>,
    pub fetched_at: DateTime<Utc>,
    /// 업체 호출이 실패해 예전 값을 돌려준다
    pub stale: bool,
    /// 이번 조회가 캐시를 어떻게 썼는가 (추적 속성)
    pub source: Source,
}

/// 캐시를 어떻게 썼는가
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// TTL 안의 캐시 값
    Cache,
    /// 업체에서 새로 가져옴
    Provider,
    /// 업체 호출이 실패해 예전 캐시 값을 줌
    Stale,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cache => "hit",
            Self::Provider => "miss",
            Self::Stale => "stale",
        }
    }
}

/// 빈 시간 캐시. `Cache`는 내부에서 잠금 없이 여러 스레드가 함께 쓸 수 있다 (`Clone`도 같은 캐시를 가리킨다).
#[derive(Clone)]
pub struct AvailabilityCache {
    entries: Cache<Key, Entry>,
}

impl Default for AvailabilityCache {
    fn default() -> Self {
        Self::new()
    }
}

impl AvailabilityCache {
    pub fn new() -> Self {
        Self {
            entries: Cache::builder()
                .max_capacity(MAX_ENTRIES)
                // stale-if-error 한도가 지나면 캐시에서도 지운다 (그보다 오래된 값은 줄 일이 없다)
                .time_to_live(STALE_IF_ERROR)
                .build(),
        }
    }

    /// 빈 시간을 돌려준다 (모듈 설명의 흐름). 줄 값이 없으면 업체 호출의 실패를 그대로 돌려준다.
    pub async fn get(
        &self,
        provider: &ResilientProvider,
        resource_id: &str,
        date: NaiveDate,
    ) -> Result<Availability, ProviderError> {
        let key = Key {
            provider: provider.id().clone(),
            resource_id: resource_id.to_owned(),
            date,
        };
        let ttl = provider.policy().availability_cache_ttl();
        let cached = self.entries.get(&key);
        if let Some(entry) = &cached
            && entry.fetched.elapsed() < ttl
        {
            return Ok(answer(entry, false, Source::Cache));
        }

        match provider.availability(resource_id, date).await {
            Ok(slots) => {
                let entry = Entry {
                    slots: Arc::new(slots),
                    // API의 다른 시각처럼 마이크로초까지만 (docs/04-api.md §1)
                    fetched_at: db::normalize_timestamp(Utc::now()),
                    fetched: Instant::now(),
                };
                self.entries.insert(key, entry.clone());
                Ok(answer(&entry, false, Source::Provider))
            }
            // 업무적 거절은 업체가 정상으로 답한 것이다. 예전 값으로 덮지 않는다.
            Err(error @ ProviderError::Rejected { .. }) => Err(error),
            Err(error) => match cached {
                Some(entry) => {
                    tracing::warn!(
                        provider = %provider.id(),
                        outcome = error.outcome(),
                        age_ms = entry.fetched.elapsed().as_millis() as u64,
                        "업체 호출이 실패해 예전 빈 시간을 돌려준다 (stale-if-error)"
                    );
                    Ok(answer(&entry, true, Source::Stale))
                }
                None => Err(error),
            },
        }
    }
}

fn answer(entry: &Entry, stale: bool, source: Source) -> Availability {
    Availability {
        slots: entry.slots.clone(),
        fetched_at: entry.fetched_at,
        stale,
        source,
    }
}
