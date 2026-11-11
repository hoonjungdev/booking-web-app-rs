//! 적응형 호출 속도 조절 (AIMD, docs/03-resilience.md §6 ④)
//!
//! 업체의 실제 한도를 몰라도 스스로 맞춰 간다. TCP 혼잡 제어와 같은 방식이다.
//!
//! ```text
//! 업체가 한도 초과를 알림 (429, 503 + Retry-After) ──▶ 허용 속도 ÷ 2        (Multiplicative Decrease)
//! 5초 동안 한도 초과 없이 호출이 이어짐            ──▶ 허용 속도 + 상한의 10% (Additive Increase), 상한까지
//! ```
//! 줄일 때는 크게, 늘릴 때는 조금씩 움직이므로 업체 한도 근처에서 안정된다.
//!
//! 현재 시각(`now`)을 인자로 받는 순수한 상태 머신이다 (서킷 브레이커와 같은 방식). 실제로 기다리지 않고
//! 시간 흐름을 흉내 내 단위 테스트한다. 속도가 바뀌면 `ResilientProvider`가 rate limiter를 새 속도로 다시 만든다.

use std::time::{Duration, Instant};

/// 한 번 줄인 뒤 이 시간 안에 또 온 한도 초과 신호는 무시한다.
/// 이미 나가 있던 여러 호출이 한꺼번에 429를 받아도 한 번만 줄이기 위해서다 (그러지 않으면 순식간에 바닥까지 떨어진다).
pub const DECREASE_COOLDOWN: Duration = Duration::from_secs(1);
/// 한도 초과 없이 이만큼 지나면 조금 올린다
pub const INCREASE_INTERVAL: Duration = Duration::from_secs(5);
/// 가장 낮은 허용 속도 (초당 호출 수)
pub const MIN_RATE: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveRate {
    /// 정책이 정한 상한 (`rate_limit.per_second`)
    ceiling: u32,
    /// 지금 허용하는 초당 호출 수
    current: u32,
    /// 정책에서 끈 경우(`rate_limit.adaptive = false`) 상한에 머문다
    enabled: bool,
    /// 마지막으로 줄인 시각
    last_decrease: Option<Instant>,
    /// 마지막 한도 초과(또는 마지막 변경) 이후 "조용한" 구간이 시작된 시각
    calm_since: Instant,
}

impl AdaptiveRate {
    pub fn new(ceiling: u32, enabled: bool, now: Instant) -> Self {
        let ceiling = ceiling.max(MIN_RATE);
        Self {
            ceiling,
            current: ceiling,
            enabled,
            last_decrease: None,
            calm_since: now,
        }
    }

    /// 지금 허용하는 초당 호출 수
    pub fn current(&self) -> u32 {
        self.current
    }

    /// 업체가 한도 초과를 알렸다. 속도를 바꿨으면 새 속도를 돌려준다.
    pub fn on_rate_limited(&mut self, now: Instant) -> Option<u32> {
        // 한도 초과가 났으니 "조용한 시간"을 처음부터 다시 잰다
        self.calm_since = now;
        if !self.enabled {
            return None;
        }
        let cooling = self
            .last_decrease
            .is_some_and(|at| now.saturating_duration_since(at) < DECREASE_COOLDOWN);
        if cooling {
            return None;
        }
        self.last_decrease = Some(now);
        self.change((self.current / 2).max(MIN_RATE))
    }

    /// 업체를 불렀고 한도 초과는 아니었다. 한동안 조용했으면 조금 올린다. 속도를 바꿨으면 새 속도를 돌려준다.
    pub fn on_calm(&mut self, now: Instant) -> Option<u32> {
        if !self.enabled || self.current >= self.ceiling {
            return None;
        }
        if now.saturating_duration_since(self.calm_since) < INCREASE_INTERVAL {
            return None;
        }
        self.calm_since = now;
        // 상한의 10%씩 (적어도 1). 상한이 얼마든 절반에서 상한까지 다섯 번이면 돌아온다.
        let step = (self.ceiling / 10).max(1);
        self.change(self.current.saturating_add(step).min(self.ceiling))
    }

    /// 지금 속도에 맞춘 버킷 크기: 상한일 때의 버킷 크기(`burst`)를 속도와 같은 비율로 줄인다 (적어도 1).
    /// 속도만 줄이고 버킷을 그대로 두면, 버킷이 가득 찼을 때 한꺼번에 나가는 호출이 다시 한도를 넘는다.
    pub fn scaled_burst(&self, burst: u32) -> u32 {
        let scaled = u64::from(burst) * u64::from(self.current) / u64::from(self.ceiling);
        u32::try_from(scaled).unwrap_or(u32::MAX).max(1)
    }

    fn change(&mut self, next: u32) -> Option<u32> {
        if next == self.current {
            return None;
        }
        self.current = next;
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn 한도_초과를_알리면_절반으로_줄인다() {
        let t0 = Instant::now();
        let mut rate = AdaptiveRate::new(10, true, t0);
        assert_eq!(rate.on_rate_limited(t0), Some(5));
        assert_eq!(rate.current(), 5);
        assert_eq!(rate.on_rate_limited(t0 + 2 * SECOND), Some(2));
        assert_eq!(rate.on_rate_limited(t0 + 4 * SECOND), Some(1));
        assert_eq!(
            rate.on_rate_limited(t0 + 6 * SECOND),
            None,
            "1 밑으로는 내려가지 않는다"
        );
    }

    #[test]
    fn 잇달아_온_한도_초과는_한_번만_센다() {
        let t0 = Instant::now();
        let mut rate = AdaptiveRate::new(16, true, t0);
        assert_eq!(rate.on_rate_limited(t0), Some(8));
        // 이미 나가 있던 호출들이 같은 순간에 429를 받았다
        assert_eq!(rate.on_rate_limited(t0 + Duration::from_millis(10)), None);
        assert_eq!(rate.on_rate_limited(t0 + Duration::from_millis(900)), None);
        assert_eq!(rate.current(), 8);
        assert_eq!(rate.on_rate_limited(t0 + SECOND), Some(4));
    }

    #[test]
    fn 한동안_조용하면_상한의_10퍼센트씩_되돌린다() {
        let t0 = Instant::now();
        let mut rate = AdaptiveRate::new(20, true, t0);
        rate.on_rate_limited(t0);
        assert_eq!(rate.current(), 10);

        assert_eq!(
            rate.on_calm(t0 + 4 * SECOND),
            None,
            "5초가 지나기 전에는 그대로"
        );
        assert_eq!(rate.on_calm(t0 + 5 * SECOND), Some(12));
        assert_eq!(rate.on_calm(t0 + 6 * SECOND), None, "다음 5초를 다시 센다");
        assert_eq!(rate.on_calm(t0 + 10 * SECOND), Some(14));
        let mut now = t0 + 10 * SECOND;
        for _ in 0..10 {
            now += INCREASE_INTERVAL;
            rate.on_calm(now);
        }
        assert_eq!(rate.current(), 20, "상한을 넘지 않는다");
    }

    #[test]
    fn 회복하는_도중에_한도_초과가_오면_조용한_시간을_다시_잰다() {
        let t0 = Instant::now();
        let mut rate = AdaptiveRate::new(10, true, t0);
        rate.on_rate_limited(t0);
        // 쿨다운 안이라 줄이지는 않지만, 조용한 시간은 처음부터 다시 잰다
        rate.on_rate_limited(t0 + Duration::from_millis(500));
        assert_eq!(rate.on_calm(t0 + 5 * SECOND), None);
        assert_eq!(rate.on_calm(t0 + Duration::from_millis(5_500)), Some(6));
    }

    #[test]
    fn 끄면_상한에_머문다() {
        let t0 = Instant::now();
        let mut rate = AdaptiveRate::new(10, false, t0);
        assert_eq!(rate.on_rate_limited(t0), None);
        assert_eq!(rate.current(), 10);
    }

    #[test]
    fn 버킷도_속도와_같은_비율로_줄인다() {
        let t0 = Instant::now();
        let mut rate = AdaptiveRate::new(10, true, t0);
        assert_eq!(rate.scaled_burst(6), 6);
        rate.on_rate_limited(t0);
        assert_eq!(rate.scaled_burst(6), 3);
        rate.on_rate_limited(t0 + 2 * SECOND);
        rate.on_rate_limited(t0 + 4 * SECOND);
        assert_eq!(rate.current(), 1);
        assert_eq!(rate.scaled_burst(6), 1, "적어도 1");
    }
}
