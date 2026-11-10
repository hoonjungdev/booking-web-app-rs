//! 업체별 장애 대응 정책 (`providers.policy` jsonb, docs/03-resilience.md §3)
//!
//! JSON의 숫자 필드 이름(`_ms`)을 그대로 두고, 코드에서는 `Duration`을 돌려주는 메서드로 꺼내 쓴다.
//! `#[serde(default)]`: JSON에 없는 필드는 `Default` 값(문서 §3의 기본값)으로 채운다. 모르는 필드는 무시한다.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::BuildError;

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
    /// 초당 허용 호출 수
    pub per_second: u32,
    /// 한꺼번에 몰아서 보낼 수 있는 호출 수
    pub burst: u32,
    /// 토큰을 기다리는 최대 시간. 넘으면 호출하지 않고 `Throttled`
    pub max_wait_ms: u64,
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
            "rate_limit":      { "per_second": 10, "burst": 5, "max_wait_ms": 500 },
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
