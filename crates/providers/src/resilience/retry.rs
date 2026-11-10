//! 호출 내 재시도 규칙 (docs/03-resilience.md §1 표, §4)

use std::time::Duration;

use crate::ProviderError;

/// 업체가 `Retry-After`로 이보다 오래 쉬라고 하면 같은 호출 안에서는 다시 시도하지 않는다
pub const MAX_IN_CALL_RETRY_AFTER: Duration = Duration::from_secs(1);

/// 이 실패 뒤에 같은 호출 안에서 다시 시도해도 되는가.
///
/// 되면 `Some(최소 대기 시간)`, 안 되면 `None`. `idempotent_safe`는 다시 보내도 업체에 중복이 생기지 않는
/// 호출인지(조회 호출이거나, 업체가 멱등키를 지원하는 생성 · 취소 호출)를 뜻한다.
pub fn may_retry(error: &ProviderError, idempotent_safe: bool) -> Option<Duration> {
    match error {
        // 요청이 업체에 가지 않았으므로 언제나 안전하다
        ProviderError::Connect { .. } => Some(Duration::ZERO),
        // 업체가 처리했을 수도 있다. 다시 보내도 중복이 생기지 않을 때만 재시도한다
        ProviderError::Timeout | ProviderError::Transport { .. } | ProviderError::Server { .. } => {
            idempotent_safe.then_some(Duration::ZERO)
        }
        // 업체가 알려 준 시간이 짧을 때만 그만큼 기다렸다가 다시 시도한다
        ProviderError::RateLimited {
            retry_after: Some(wait),
        } if *wait <= MAX_IN_CALL_RETRY_AFTER => Some(*wait),
        // 업무적 거절과 4xx는 다시 보내도 같은 결과다. 응답 해석 실패는 업체 계약 변경일 수 있어 알림 대상.
        // 우리 쪽에서 막은 것(서킷 · 대기 한도)은 작업 수준에서 다시 예약한다.
        ProviderError::RateLimited { .. }
        | ProviderError::Rejected { .. }
        | ProviderError::Client { .. }
        | ProviderError::Decode { .. }
        | ProviderError::CircuitOpen { .. }
        | ProviderError::Throttled { .. } => None,
    }
}

/// `attempt`번째 시도가 실패한 뒤 기다릴 시간 (지수 백오프 + full jitter).
///
/// `delay = random(0, min(max, base × 2^(attempt-1)))`. 무작위 폭을 0부터 잡는 full jitter는
/// 같은 순간에 실패한 호출들이 같은 순간에 다시 몰리지 않게 흩어 놓는 효과가 가장 크다.
/// `random`은 0.0~1.0 사이의 값이다. 밖에서 받기 때문에 테스트에서 결과를 정확히 확인할 수 있다.
pub fn backoff_delay(attempt: u32, base: Duration, max: Duration, random: f64) -> Duration {
    let exponent = attempt.saturating_sub(1).min(20);
    let ceiling = base.saturating_mul(1 << exponent).min(max);
    ceiling.mul_f64(random.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use domain::RejectReason;

    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn 연결_실패는_멱등키가_없어도_재시도한다() {
        let error = ProviderError::Connect {
            detail: "refused".into(),
        };
        assert_eq!(may_retry(&error, false), Some(Duration::ZERO));
    }

    #[test]
    fn 처리_여부를_모르는_실패는_멱등_안전할_때만_재시도한다() {
        let ambiguous = [
            ProviderError::Timeout,
            ProviderError::Transport {
                detail: "reset".into(),
            },
            ProviderError::Server { status: 502 },
        ];
        for error in ambiguous {
            assert_eq!(may_retry(&error, true), Some(Duration::ZERO), "{error:?}");
            assert_eq!(
                may_retry(&error, false),
                None,
                "{error:?}: 멱등키 없이 다시 보내면 이중 예약 위험"
            );
        }
    }

    #[test]
    fn 호출_제한은_retry_after가_1초_이하일_때만_그만큼_기다렸다가_재시도한다() {
        let short = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(1)),
        };
        assert_eq!(may_retry(&short, false), Some(Duration::from_secs(1)));

        let long = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(2)),
        };
        assert_eq!(may_retry(&long, true), None);
        let unknown = ProviderError::RateLimited { retry_after: None };
        assert_eq!(may_retry(&unknown, true), None);
    }

    #[test]
    fn 다시_보내도_소용없는_실패는_재시도하지_않는다() {
        let now = Instant::now();
        let never = [
            ProviderError::Rejected {
                reason: RejectReason::SlotUnavailable,
            },
            ProviderError::Client {
                status: 422,
                body: String::new(),
            },
            ProviderError::Decode {
                detail: "bad json".into(),
            },
            ProviderError::CircuitOpen { retry_at: now },
            ProviderError::Throttled { retry_at: now },
        ];
        for error in never {
            assert_eq!(may_retry(&error, true), None, "{error:?}");
        }
    }

    #[test]
    fn 백오프는_두_배씩_늘고_상한을_넘지_않는다() {
        let base = 200 * MS;
        let max = 2_000 * MS;
        assert_eq!(backoff_delay(1, base, max, 1.0), 200 * MS);
        assert_eq!(backoff_delay(2, base, max, 1.0), 400 * MS);
        assert_eq!(backoff_delay(3, base, max, 1.0), 800 * MS);
        assert_eq!(backoff_delay(5, base, max, 1.0), 2_000 * MS);
        assert_eq!(backoff_delay(u32::MAX, base, max, 1.0), 2_000 * MS);
    }

    #[test]
    fn 백오프는_0부터_상한_사이의_무작위_값이다() {
        let base = 200 * MS;
        let max = 2_000 * MS;
        assert_eq!(backoff_delay(2, base, max, 0.0), Duration::ZERO);
        assert_eq!(backoff_delay(2, base, max, 0.5), 200 * MS);
        // 범위를 벗어난 값은 0~1로 자른다
        assert_eq!(backoff_delay(2, base, max, 7.0), 400 * MS);
        assert_eq!(backoff_delay(2, base, max, -1.0), Duration::ZERO);
    }
}
