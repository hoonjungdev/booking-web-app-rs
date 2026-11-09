//! 토큰 버킷 rate limiter.
//!
//! 버킷에 토큰이 최대 `burst`개까지 쌓이고, 초당 `per_second`개씩 다시 채워진다.
//! 요청 하나가 토큰 하나를 쓰고, 토큰이 없으면 거절한다.
//!
//! 현재 시각(`now`)을 인자로 받기 때문에 테스트에서 실제로 기다리지 않고 시간 흐름을 흉내 낼 수 있다.

use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct TokenBucket {
    per_second: f64,
    capacity: f64,
    tokens: f64,
    updated_at: Instant,
}

/// 토큰 요청 결과
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquire {
    /// 허용. `remaining`은 이번 요청 뒤 남은 토큰 수
    Granted { remaining: u32 },
    /// 거절. `retry_after`만큼 기다리면 토큰 하나가 생긴다
    Denied { retry_after: Duration },
}

impl TokenBucket {
    /// 가득 찬 버킷을 만든다. `per_second`와 `burst`가 0이면 1로 올린다.
    pub fn new(per_second: u32, burst: u32, now: Instant) -> Self {
        let capacity = f64::from(burst.max(1));
        Self {
            per_second: f64::from(per_second.max(1)),
            capacity,
            tokens: capacity,
            updated_at: now,
        }
    }

    /// 토큰 하나를 쓰려고 시도한다.
    pub fn try_acquire(&mut self, now: Instant) -> Acquire {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Acquire::Granted {
                remaining: self.tokens.floor() as u32,
            }
        } else {
            let missing = 1.0 - self.tokens;
            Acquire::Denied {
                retry_after: Duration::from_secs_f64(missing / self.per_second),
            }
        }
    }

    /// 버킷이 다시 가득 찰 때까지 걸리는 시간
    pub fn time_until_full(&self) -> Duration {
        Duration::from_secs_f64((self.capacity - self.tokens).max(0.0) / self.per_second)
    }

    /// 초당 허용 요청 수 (응답 헤더 `X-RateLimit-Limit`에 쓴다)
    pub fn limit(&self) -> u32 {
        self.per_second as u32
    }

    fn refill(&mut self, now: Instant) {
        // saturating_duration_since: now가 더 이르면(시계가 뒤로 간 경우) 0을 돌려준다.
        let elapsed = now.saturating_duration_since(self.updated_at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.per_second).min(self.capacity);
        self.updated_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst만큼_연속으로_허용하고_그다음은_거절한다() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(10, 2, t0);

        assert_eq!(bucket.try_acquire(t0), Acquire::Granted { remaining: 1 });
        assert_eq!(bucket.try_acquire(t0), Acquire::Granted { remaining: 0 });
        assert!(matches!(bucket.try_acquire(t0), Acquire::Denied { .. }));
    }

    #[test]
    fn 시간이_지나면_토큰이_다시_찬다() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(10, 1, t0);
        assert!(matches!(bucket.try_acquire(t0), Acquire::Granted { .. }));

        // 초당 10개 → 100ms마다 1개
        let Acquire::Denied { retry_after } = bucket.try_acquire(t0) else {
            panic!("토큰이 없어야 한다");
        };
        assert_eq!(retry_after, Duration::from_millis(100));

        let later = t0 + Duration::from_millis(100);
        assert!(matches!(bucket.try_acquire(later), Acquire::Granted { .. }));
    }

    #[test]
    fn 토큰은_capacity를_넘지_않는다() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(100, 3, t0);
        let much_later = t0 + Duration::from_secs(60);
        assert_eq!(
            bucket.try_acquire(much_later),
            Acquire::Granted { remaining: 2 }
        );
    }
}
