//! 작업 재시도 간격 (docs/03-resilience.md §4 "작업 재예약")

use std::time::Duration;

const BASE_SECS: f64 = 5.0;
const MAX_SECS: f64 = 600.0;

/// `attempt`번째 시도가 실패한 뒤 기다릴 시간.
///
/// 상한은 5초 × 2^(attempt-1)이고 최대 10분이다. 그 절반은 고정이고 나머지 절반은 무작위다 (equal jitter).
/// 무작위를 섞는 이유: 장애가 풀린 순간 밀린 작업이 모두 같은 시각에 몰려 업체를 다시 쓰러뜨리지 않게 하려고.
/// `jitter`는 0.0~1.0 사이의 무작위 값이다. 밖에서 넘겨받기 때문에 테스트에서 결과를 정확히 확인할 수 있다.
pub fn job_backoff(attempt: i32, jitter: f64) -> Duration {
    let exponent = attempt.saturating_sub(1).clamp(0, 20);
    let ceiling = (BASE_SECS * 2f64.powi(exponent)).min(MAX_SECS);
    let half = ceiling / 2.0;
    Duration::from_secs_f64(half + half * jitter.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 시도할수록_두_배씩_늘고_10분을_넘지_않는다() {
        assert_eq!(job_backoff(1, 1.0), Duration::from_secs(5));
        assert_eq!(job_backoff(2, 1.0), Duration::from_secs(10));
        assert_eq!(job_backoff(3, 1.0), Duration::from_secs(20));
        assert_eq!(job_backoff(20, 1.0), Duration::from_secs(600));
    }

    #[test]
    fn 무작위_값에_따라_상한의_절반에서_상한_사이가_된다() {
        assert_eq!(job_backoff(2, 0.0), Duration::from_secs(5));
        assert_eq!(job_backoff(2, 0.5), Duration::from_secs_f64(7.5));
        // 범위를 벗어난 값은 0~1로 자른다
        assert_eq!(job_backoff(2, 7.0), Duration::from_secs(10));
        assert_eq!(job_backoff(0, -1.0), Duration::from_secs_f64(2.5));
    }
}
