//! 업체별 장애 대응 정책 (`providers.policy` jsonb, docs/03-resilience.md §3)
//!
//! JSON의 숫자 필드 이름(`_ms`)을 그대로 두고, 코드에서는 `Duration`을 돌려주는 메서드로 꺼내 쓴다.
//! `#[serde(default)]`: JSON에 없는 필드는 `Default` 값(문서 §3의 기본값)으로 채운다. 모르는 필드는 무시한다.
//!
//! 정책은 실행 중에 바뀔 수 있다 (관리자 화면에서 편집, Phase 6). `ResilientProvider`가 새 정책을 받으면
//! 정책 · rate limiter · bulkhead를 통째로 교체하고, 진행 중인 호출은 시작할 때의 정책을 끝까지 쓴다.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::BuildError;

/// 시도 타임아웃의 상한 (밀리초). 어댑터의 HTTP 클라이언트가 안전장치로 거는 타임아웃(`ADAPTER_TIMEOUT`)과 같다.
/// 이보다 길게 정하면 안전장치가 먼저 끊어 정책 값이 지켜지지 않으므로 거절한다.
pub const MAX_ATTEMPT_TIMEOUT_MS: u64 = 60_000;
/// 시간 값(밀리초)의 상한: 1시간. 이보다 긴 값은 뜻이 없고, "지금 + 값" 같은 시각 계산이 넘쳐 panic이 날 수 있다.
pub const MAX_DURATION_MS: u64 = 60 * 60 * 1000;
/// 빈 시간 캐시를 새것으로 보는 시간의 상한 (밀리초). 캐시는 항목을 최대 5분(stale-if-error 한도)까지만 둔다.
pub const MAX_AVAILABILITY_CACHE_TTL_MS: u64 = 5 * 60 * 1000;
/// 개수 값(초당 호출 수, 버킷 크기, 동시 실행 수, 서킷 창 크기, 시험 호출 수)의 상한
pub const MAX_COUNT: u32 = 100_000;
/// 호출 한 번 안의 최대 시도 횟수 상한
pub const MAX_RETRY_ATTEMPTS: u32 = 10;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderPolicy {
    /// 시도 한 번의 최대 시간
    pub attempt_timeout_ms: u64,
    /// 호출 한 번(재시도 포함)에 쓸 수 있는 전체 시간
    pub call_budget_ms: u64,
    pub retry: RetryPolicy,
    pub rate_limit: RateLimitPolicy,
    pub bulkhead: BulkheadPolicy,
    pub circuit_breaker: CircuitBreakerPolicy,
    /// 빈 시간 조회 결과를 캐시에서 새것으로 보는 시간. 지나면 업체에 다시 묻고, 실패하면 5분까지는 캐시 값을 준다 (stale-if-error)
    pub availability_cache_ttl_ms: u64,
}

/// 호출 내 재시도 (지수 백오프 + full jitter)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryPolicy {
    /// 첫 시도를 포함한 최대 시도 횟수. 1이면 재시도하지 않는다
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

/// 우리 쪽에서 먼저 거는 호출 속도 제한
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitPolicy {
    /// 초당 허용 호출 수. 적응형(`adaptive`)이면 이 값이 상한이다
    pub per_second: u32,
    /// 한꺼번에 몰아서 보낼 수 있는 호출 수
    pub burst: u32,
    /// 토큰을 기다리는 최대 시간. 넘으면 호출하지 않고 `Throttled`
    pub max_wait_ms: u64,
    /// 적응형 조절 (AIMD): 업체가 한도 초과를 알리면 허용 속도를 절반으로, 한동안 문제없으면 조금씩 되돌린다
    /// (docs/03-resilience.md §6 ④)
    pub adaptive: bool,
}

/// 업체별 동시 호출 수 제한
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BulkheadPolicy {
    pub max_concurrency: u32,
    /// 자리를 기다리는 최대 시간. 넘으면 호출하지 않고 `Throttled`
    pub max_wait_ms: u64,
}

/// 서킷 브레이커 (docs/03-resilience.md §7)
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CircuitBreakerPolicy {
    /// 최근 몇 번의 호출 결과로 실패율을 계산할지
    pub window: u32,
    /// 이만큼은 호출해 봐야 실패율로 판단한다 (호출 몇 번으로 성급하게 열리지 않도록)
    pub min_calls: u32,
    /// 이 비율 이상 실패하면 연다 (0.0 ~ 1.0)
    pub failure_rate: f64,
    /// 열린 뒤 시험 호출을 허용하기까지 기다리는 시간
    pub open_ms: u64,
    /// 반만 열린(half-open) 상태에서 허용하는 시험 호출 수. 모두 성공해야 닫힌다
    pub half_open_probes: u32,
}

impl Default for ProviderPolicy {
    fn default() -> Self {
        Self {
            attempt_timeout_ms: 3_000,
            call_budget_ms: 15_000,
            retry: RetryPolicy::default(),
            rate_limit: RateLimitPolicy::default(),
            bulkhead: BulkheadPolicy::default(),
            circuit_breaker: CircuitBreakerPolicy::default(),
            availability_cache_ttl_ms: 30_000,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay_ms: 200,
            max_delay_ms: 2_000,
        }
    }
}

impl Default for RateLimitPolicy {
    fn default() -> Self {
        Self {
            per_second: 10,
            burst: 5,
            max_wait_ms: 500,
            adaptive: true,
        }
    }
}

impl Default for BulkheadPolicy {
    fn default() -> Self {
        Self {
            max_concurrency: 8,
            max_wait_ms: 200,
        }
    }
}

impl Default for CircuitBreakerPolicy {
    fn default() -> Self {
        Self {
            window: 20,
            min_calls: 10,
            failure_rate: 0.5,
            open_ms: 30_000,
            half_open_probes: 3,
        }
    }
}

impl ProviderPolicy {
    /// JSON을 읽고 값이 말이 되는지 검사한다
    pub fn from_json(value: serde_json::Value) -> Result<Self, BuildError> {
        let policy: Self = serde_json::from_value(value)?;
        policy.validate().map_err(BuildError::InvalidPolicy)?;
        Ok(policy)
    }

    /// 0이면 동작할 수 없는 값, 범위를 벗어난 비율 등을 거른다. 문제가 있으면 이유를 담은 `Err`.
    pub fn validate(&self) -> Result<(), String> {
        let positive = [
            ("attempt_timeout_ms", self.attempt_timeout_ms),
            ("call_budget_ms", self.call_budget_ms),
            ("retry.max_attempts", u64::from(self.retry.max_attempts)),
            (
                "rate_limit.per_second",
                u64::from(self.rate_limit.per_second),
            ),
            ("rate_limit.burst", u64::from(self.rate_limit.burst)),
            (
                "bulkhead.max_concurrency",
                u64::from(self.bulkhead.max_concurrency),
            ),
            (
                "circuit_breaker.window",
                u64::from(self.circuit_breaker.window),
            ),
            (
                "circuit_breaker.min_calls",
                u64::from(self.circuit_breaker.min_calls),
            ),
            ("circuit_breaker.open_ms", self.circuit_breaker.open_ms),
            (
                "circuit_breaker.half_open_probes",
                u64::from(self.circuit_breaker.half_open_probes),
            ),
        ];
        if let Some((name, _)) = positive.iter().find(|(_, value)| *value == 0) {
            return Err(format!("{name}는 1 이상이어야 합니다"));
        }
        let durations = [
            ("call_budget_ms", self.call_budget_ms),
            ("retry.base_delay_ms", self.retry.base_delay_ms),
            ("retry.max_delay_ms", self.retry.max_delay_ms),
            ("rate_limit.max_wait_ms", self.rate_limit.max_wait_ms),
            ("bulkhead.max_wait_ms", self.bulkhead.max_wait_ms),
            ("circuit_breaker.open_ms", self.circuit_breaker.open_ms),
        ];
        if let Some((name, _)) = durations.iter().find(|(_, value)| *value > MAX_DURATION_MS) {
            return Err(format!("{name}는 {MAX_DURATION_MS}(1시간) 이하여야 합니다"));
        }
        let counts = [
            ("rate_limit.per_second", self.rate_limit.per_second),
            ("rate_limit.burst", self.rate_limit.burst),
            ("bulkhead.max_concurrency", self.bulkhead.max_concurrency),
            ("circuit_breaker.window", self.circuit_breaker.window),
            (
                "circuit_breaker.half_open_probes",
                self.circuit_breaker.half_open_probes,
            ),
        ];
        if let Some((name, _)) = counts.iter().find(|(_, value)| *value > MAX_COUNT) {
            return Err(format!("{name}는 {MAX_COUNT} 이하여야 합니다"));
        }
        if self.retry.max_attempts > MAX_RETRY_ATTEMPTS {
            return Err(format!(
                "retry.max_attempts는 {MAX_RETRY_ATTEMPTS} 이하여야 합니다"
            ));
        }
        if self.availability_cache_ttl_ms > MAX_AVAILABILITY_CACHE_TTL_MS {
            return Err(format!(
                "availability_cache_ttl_ms는 {MAX_AVAILABILITY_CACHE_TTL_MS}(5분) 이하여야 합니다 (캐시는 항목을 5분까지만 둔다)"
            ));
        }
        if self.attempt_timeout_ms > MAX_ATTEMPT_TIMEOUT_MS {
            return Err(format!(
                "attempt_timeout_ms는 {MAX_ATTEMPT_TIMEOUT_MS} 이하여야 합니다 (어댑터의 HTTP 안전장치 타임아웃)"
            ));
        }
        if self.retry.base_delay_ms > self.retry.max_delay_ms {
            return Err("retry.base_delay_ms는 retry.max_delay_ms보다 클 수 없습니다".into());
        }
        let breaker = &self.circuit_breaker;
        if breaker.min_calls > breaker.window {
            return Err(
                "circuit_breaker.min_calls는 circuit_breaker.window보다 클 수 없습니다".into(),
            );
        }
        if !(breaker.failure_rate > 0.0 && breaker.failure_rate <= 1.0) {
            return Err(format!(
                "circuit_breaker.failure_rate는 0보다 크고 1 이하여야 합니다 (입력: {})",
                breaker.failure_rate
            ));
        }
        Ok(())
    }

    pub fn attempt_timeout(&self) -> Duration {
        Duration::from_millis(self.attempt_timeout_ms)
    }

    pub fn call_budget(&self) -> Duration {
        Duration::from_millis(self.call_budget_ms)
    }

    /// 호출 한 번이 걸릴 수 있는 최대 시간: 호출 예산 + 마지막 시도의 관문 대기(rate limiter + bulkhead).
    /// 워커의 lease는 이보다 길어야 한다. 짧으면 호출 도중 lease가 끝나 다른 워커가 같은 작업을 다시 가져간다.
    pub fn longest_call(&self) -> Duration {
        self.call_budget() + self.rate_limit.max_wait() + self.bulkhead.max_wait()
    }

    pub fn availability_cache_ttl(&self) -> Duration {
        Duration::from_millis(self.availability_cache_ttl_ms)
    }
}

impl RetryPolicy {
    pub fn base_delay(&self) -> Duration {
        Duration::from_millis(self.base_delay_ms)
    }

    pub fn max_delay(&self) -> Duration {
        Duration::from_millis(self.max_delay_ms)
    }
}

impl RateLimitPolicy {
    pub fn max_wait(&self) -> Duration {
        Duration::from_millis(self.max_wait_ms)
    }
}

impl BulkheadPolicy {
    pub fn max_wait(&self) -> Duration {
        Duration::from_millis(self.max_wait_ms)
    }
}

impl CircuitBreakerPolicy {
    pub fn open_duration(&self) -> Duration {
        Duration::from_millis(self.open_ms)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn 기본값은_설계_문서와_같다() {
        let documented = json!({
            "attempt_timeout_ms": 3000,
            "call_budget_ms": 15000,
            "retry":           { "max_attempts": 3, "base_delay_ms": 200, "max_delay_ms": 2000 },
            "rate_limit":      { "per_second": 10, "burst": 5, "max_wait_ms": 500, "adaptive": true },
            "bulkhead":        { "max_concurrency": 8, "max_wait_ms": 200 },
            "circuit_breaker": { "window": 20, "min_calls": 10, "failure_rate": 0.5,
                                 "open_ms": 30000, "half_open_probes": 3 },
            "availability_cache_ttl_ms": 30000
        });
        assert_eq!(
            ProviderPolicy::from_json(documented).unwrap(),
            ProviderPolicy::default()
        );
    }

    #[test]
    fn 빠진_필드는_기본값으로_채운다() {
        let policy = ProviderPolicy::from_json(
            json!({ "attempt_timeout_ms": 500, "retry": { "max_attempts": 1 } }),
        )
        .unwrap();
        assert_eq!(policy.attempt_timeout_ms, 500);
        assert_eq!(policy.retry.max_attempts, 1);
        assert_eq!(policy.retry.base_delay_ms, 200);
        assert_eq!(policy.circuit_breaker, CircuitBreakerPolicy::default());
        assert!(
            policy.rate_limit.adaptive,
            "적응형 조절은 기본으로 켜져 있다"
        );
        assert_eq!(policy.availability_cache_ttl_ms, 30_000);
    }

    #[test]
    fn 호출_한_번의_최대_시간은_예산과_관문_대기를_더한_값이다() {
        // 15초 + rate limiter 0.5초 + bulkhead 0.2초
        assert_eq!(
            ProviderPolicy::default().longest_call(),
            Duration::from_millis(15_700)
        );
    }

    #[test]
    fn 동작할_수_없는_값은_거절한다() {
        let invalid = [
            json!({ "retry": { "max_attempts": 0 } }),
            json!({ "rate_limit": { "per_second": 0 } }),
            json!({ "retry": { "base_delay_ms": 5000, "max_delay_ms": 100 } }),
            json!({ "circuit_breaker": { "window": 5, "min_calls": 10 } }),
            json!({ "circuit_breaker": { "failure_rate": 1.5 } }),
            json!({ "circuit_breaker": { "failure_rate": 0.0 } }),
            json!({ "attempt_timeout_ms": 60_001 }),
            // 너무 큰 값: 시각 계산이 넘치거나(panic) 뜻이 없는 값
            json!({ "circuit_breaker": { "open_ms": 9_000_000_000_000_000_u64 } }),
            json!({ "call_budget_ms": 3_600_001 }),
            json!({ "rate_limit": { "burst": 3_000_000_000_u64 } }),
            json!({ "bulkhead": { "max_concurrency": 100_001 } }),
            json!({ "retry": { "max_attempts": 11 } }),
            json!({ "availability_cache_ttl_ms": 300_001 }),
        ];
        for value in invalid {
            assert!(
                matches!(
                    ProviderPolicy::from_json(value.clone()),
                    Err(BuildError::InvalidPolicy(_))
                ),
                "{value}"
            );
        }
        assert!(matches!(
            ProviderPolicy::from_json(json!({ "retry": { "max_attempts": -1 } })),
            Err(BuildError::Policy(_))
        ));
    }
}
