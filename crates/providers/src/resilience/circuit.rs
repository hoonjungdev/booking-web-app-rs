//! 서킷 브레이커 (docs/03-resilience.md §7)
//!
//! ```text
//! Closed ──(최근 window회 중 실패율 ≥ failure_rate, 최소 min_calls회)──▶ Open
//! Open ──(open_ms 경과)──▶ HalfOpen
//! HalfOpen ──(시험 호출 half_open_probes회 모두 성공)──▶ Closed
//! HalfOpen ──(시험 호출 중 1회라도 실패)──▶ Open
//! ```
//!
//! 현재 시각(`now`)을 인자로 받는 순수한 상태 머신이다. I/O도 잠금도 없어서 실제로 기다리지 않고
//! 시간 흐름을 흉내 내 모든 전이를 단위 테스트할 수 있다. 여러 태스크가 함께 쓰도록 잠그는 일은
//! `ResilientProvider`가 `Mutex`로 맡는다.
//!
//! 관리자가 상태를 강제할 수도 있다 ([`CircuitBreaker::control`], Phase 5). 강제된 동안에는 호출 결과를 세지 않고,
//! `open_ms`가 지나도 상태가 저절로 바뀌지 않는다. `reset`으로 풀면 닫힌 상태에서 처음부터 다시 센다.
//! (≈ Polly의 `Isolate()` / `Reset()`, Resilience4j의 forced-open · disabled 상태)

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::{CircuitBreakerPolicy, CircuitVerdict};

/// 반만 열린 상태에서 시험 호출 자리가 모두 찼을 때, 다시 와 보라고 알려 줄 최대 대기 시간
const HALF_OPEN_BUSY_RETRY: Duration = Duration::from_secs(1);

/// 관리자의 서킷 조작 (`POST /admin/api/providers/{id}/circuit`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitAction {
    /// 강제로 연다. 풀 때까지(`Reset`) 호출하지 않는다 (업체 점검 공지에 대응)
    ForceOpen,
    /// 강제로 닫는다. 풀 때까지 실패가 쌓여도 열리지 않는다 (업체는 정상인데 서킷이 잘못 열릴 때)
    ForceClose,
    /// 강제 상태를 풀고, 닫힌 상태에서 최근 결과를 비우고 처음부터 다시 센다
    Reset,
}

/// 관리자가 강제로 정해 둔 상태
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedState {
    Open,
    Closed,
}

impl ForcedState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

/// 지금 서킷 상태를 자세히 본 것 (관리자 화면의 업체 런타임 상태)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitSnapshot {
    pub state: CircuitState,
    /// 관리자가 강제로 정한 상태면 `Some`
    pub forced: Option<ForcedState>,
    /// 닫힌 상태: 실패율 계산에 쓰는 최근 결과 수와 그중 실패 수
    pub recent_calls: u32,
    pub recent_failures: u32,
    /// 열린 상태: 시험 호출을 시작할 수 있게 되는 시각 (강제로 열렸으면 `None`. 풀 때까지 열려 있다)
    pub open_until: Option<Instant>,
    /// 반만 열린 상태: 성공한 시험 호출 수와 결과를 기다리는 시험 호출 수
    pub probes_succeeded: u32,
    pub probes_in_flight: u32,
}

/// 밖에서 보는 서킷 상태
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// 정상. 모든 호출을 통과시키며 결과를 센다
    Closed,
    /// 차단. 호출하지 않고 바로 실패시킨다
    Open,
    /// 시험 중. 정해진 수의 시험 호출만 통과시킨다
    HalfOpen,
}

impl CircuitState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }
}

/// 호출을 허락받았다는 증표. 결과를 알릴 때([`CircuitBreaker::record`]) 돌려준다.
///
/// `generation`은 허락받을 때의 "상태 세대"다. 상태가 바뀔 때마다 세대가 1씩 늘어나므로,
/// Closed 시절에 시작한 호출의 결과가 HalfOpen이 된 뒤에 늦게 도착해도 시험 호출 결과로 잘못 세지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permit {
    generation: u64,
}

#[derive(Debug)]
enum State {
    /// `recent`: 최근 결과 (true = 실패). 오래된 것부터 앞에 있다 (≈ C#의 `Queue<bool>`)
    Closed {
        recent: VecDeque<bool>,
    },
    Open {
        until: Instant,
    },
    /// `in_flight`: 진행 중인 시험 호출 수, `succeeded`: 성공한 시험 호출 수, `since`: 시험을 시작한 시각
    HalfOpen {
        in_flight: u32,
        succeeded: u32,
        since: Instant,
    },
}

impl State {
    fn closed() -> Self {
        Self::Closed {
            recent: VecDeque::new(),
        }
    }

    fn half_open(now: Instant) -> Self {
        Self::HalfOpen {
            in_flight: 0,
            succeeded: 0,
            since: now,
        }
    }
}

/// 개수를 `u32`로 바꾼다. 창 크기(`window`)가 `u32`라서 넘칠 일은 없다.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[derive(Debug)]
pub struct CircuitBreaker {
    policy: CircuitBreakerPolicy,
    state: State,
    generation: u64,
    /// 관리자가 강제로 정한 상태. `Some`이면 호출 결과로 상태가 바뀌지 않는다.
    forced: Option<ForcedState>,
}

impl CircuitBreaker {
    pub fn new(policy: CircuitBreakerPolicy) -> Self {
        Self {
            policy,
            state: State::closed(),
            generation: 0,
            forced: None,
        }
    }

    pub fn state(&self) -> CircuitState {
        match self.state {
            State::Closed { .. } => CircuitState::Closed,
            State::Open { .. } => CircuitState::Open,
            State::HalfOpen { .. } => CircuitState::HalfOpen,
        }
    }

    /// 지금 상태를 자세히 본다. 상태를 바꾸지 않는다 (열린 지 `open_ms`가 지났어도 다음 호출이 올 때까지는 Open이다).
    pub fn snapshot(&self) -> CircuitSnapshot {
        let mut snapshot = CircuitSnapshot {
            state: self.state(),
            forced: self.forced,
            recent_calls: 0,
            recent_failures: 0,
            open_until: None,
            probes_succeeded: 0,
            probes_in_flight: 0,
        };
        match &self.state {
            State::Closed { recent } => {
                snapshot.recent_calls = count(recent.len());
                snapshot.recent_failures = count(recent.iter().filter(|failed| **failed).count());
            }
            State::Open { until } => {
                snapshot.open_until = self.forced.is_none().then_some(*until);
            }
            State::HalfOpen {
                in_flight,
                succeeded,
                ..
            } => {
                snapshot.probes_succeeded = *succeeded;
                snapshot.probes_in_flight = *in_flight;
            }
        }
        snapshot
    }

    /// 정책을 바꾼다 (관리자 정책 편집, Phase 6). 지금 상태는 그대로 두고, 이후의 판단부터 새 기준을 쓴다.
    /// 열린 서킷이 정책을 바꿨다고 닫히지는 않는다. 창(`window`)이 줄었으면 오래된 결과부터 버린다.
    pub fn update_policy(&mut self, policy: CircuitBreakerPolicy) {
        self.policy = policy;
        if let State::Closed { recent } = &mut self.state {
            while recent.len() > policy.window as usize {
                recent.pop_front();
            }
        }
    }

    /// 관리자 조작. 상태가 바뀌므로 그 전에 받은 허락의 늦은 결과는 세지 않는다 (세대가 바뀐다).
    pub fn control(&mut self, action: CircuitAction, now: Instant) {
        let (forced, state) = match action {
            CircuitAction::ForceOpen => (
                Some(ForcedState::Open),
                State::Open {
                    until: now + self.policy.open_duration(),
                },
            ),
            CircuitAction::ForceClose => (Some(ForcedState::Closed), State::closed()),
            CircuitAction::Reset => (None, State::closed()),
        };
        self.forced = forced;
        self.change(state);
    }

    /// 호출해도 되는지 묻는다. 안 되면 다시 시도해 볼 시각을 담은 `Err`.
    ///
    /// 열린 지 `open_ms`가 지났으면 여기서 HalfOpen으로 바뀌고 첫 시험 호출을 허락한다.
    pub fn try_acquire(&mut self, now: Instant) -> Result<Permit, Instant> {
        match self.forced {
            // 강제로 열려 있으면 풀 때까지 막는다. 기다리는 쪽(워커)이 너무 자주 묻지 않도록 open_ms 뒤에 다시 오라고 한다.
            Some(ForcedState::Open) => return Err(now + self.policy.open_duration()),
            Some(ForcedState::Closed) => {
                return Ok(Permit {
                    generation: self.generation,
                });
            }
            None => {}
        }
        let probes = self.policy.half_open_probes;
        let open = self.policy.open_duration();
        match self.state {
            State::Open { until } if now < until => return Err(until),
            State::Open { .. } => self.change(State::half_open(now)),
            // 시험 호출이 다 나갔는데 open_ms가 지나도록 결과가 다 돌아오지 않았다 (호출이 취소되거나 패닉한 경우).
            // 시험 자리가 영영 막히지 않도록 시험을 처음부터 다시 시작한다. 늦게 온 결과는 세대가 달라 무시된다.
            State::HalfOpen {
                in_flight,
                succeeded,
                since,
            } if in_flight + succeeded >= probes && now >= since + open => {
                self.change(State::half_open(now));
            }
            State::Closed { .. } | State::HalfOpen { .. } => {}
        }

        if let State::HalfOpen {
            in_flight,
            succeeded,
            ..
        } = &mut self.state
        {
            if *in_flight + *succeeded >= probes {
                // 시험 호출이 이미 다 나가 있다. 결과가 나올 때쯤 다시 와 보라고 한다.
                return Err(now + HALF_OPEN_BUSY_RETRY.min(self.policy.open_duration()));
            }
            *in_flight += 1;
        }
        Ok(Permit {
            generation: self.generation,
        })
    }

    /// 허락받은 뒤 시간이 흘렀다면(rate limiter · bulkhead 대기) 그사이 상태가 바뀌었는지 확인한다.
    ///
    /// 그대로면 받은 허락을 그대로 돌려주고, 바뀌었으면(예: 기다리는 동안 열림) 지금 상태로 다시 허락을 구한다.
    /// 이전 허락은 세대가 달라 아무 효과가 없으므로 따로 돌려줄 필요가 없다.
    pub fn revalidate(&mut self, permit: Permit, now: Instant) -> Result<Permit, Instant> {
        if permit.generation == self.generation {
            Ok(permit)
        } else {
            self.try_acquire(now)
        }
    }

    /// 허락받은 호출의 결과를 알린다.
    pub fn record(&mut self, permit: Permit, verdict: CircuitVerdict, now: Instant) {
        if permit.generation != self.generation || self.forced.is_some() {
            // 지금과 다른 상태일 때 시작한 호출의 늦은 결과이거나, 관리자가 상태를 강제해 둔 동안의 결과.
            // 현재 판단에 섞지 않는다.
            return;
        }
        let open_until = now + self.policy.open_duration();

        // `match &mut self.state`: 상태 안의 값을 그 자리에서 고친다. 상태 자체를 바꿔야 하면 `next`에 담아 두고
        // match가 끝난 뒤 바꾼다 (match 안에서는 self.state를 빌리고 있어서 통째로 바꿀 수 없다).
        let next = match &mut self.state {
            State::Closed { recent } => {
                let failed = match verdict {
                    CircuitVerdict::Success => false,
                    CircuitVerdict::Failure => true,
                    CircuitVerdict::Ignored => return,
                };
                recent.push_back(failed);
                while recent.len() > self.policy.window as usize {
                    recent.pop_front();
                }
                let failures = recent.iter().filter(|failed| **failed).count();
                let enough_calls = recent.len() >= self.policy.min_calls as usize;
                let rate = failures as f64 / recent.len() as f64;
                (enough_calls && rate >= self.policy.failure_rate)
                    .then_some(State::Open { until: open_until })
            }
            // Open일 때는 허락이 나가지 않으므로 세대가 같은 결과가 올 수 없다
            State::Open { .. } => None,
            State::HalfOpen {
                in_flight,
                succeeded,
                ..
            } => {
                *in_flight = in_flight.saturating_sub(1);
                match verdict {
                    CircuitVerdict::Failure => Some(State::Open { until: open_until }),
                    CircuitVerdict::Success => {
                        *succeeded += 1;
                        (*succeeded >= self.policy.half_open_probes).then(State::closed)
                    }
                    // 시험 자리를 돌려주기만 한다
                    CircuitVerdict::Ignored => None,
                }
            }
        };
        if let Some(state) = next {
            self.change(state);
        }
    }

    fn change(&mut self, state: State) {
        self.state = state;
        self.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use CircuitVerdict::{Failure, Ignored, Success};

    fn policy() -> CircuitBreakerPolicy {
        CircuitBreakerPolicy {
            window: 4,
            min_calls: 4,
            failure_rate: 0.5,
            open_ms: 1_000,
            half_open_probes: 2,
        }
    }

    /// 호출 하나를 허락받고 바로 결과를 알린다
    fn call(breaker: &mut CircuitBreaker, verdict: CircuitVerdict, now: Instant) {
        let permit = breaker.try_acquire(now).expect("호출이 허락되어야 한다");
        breaker.record(permit, verdict, now);
    }

    /// 실패율 기준을 넘겨 연다
    fn open(breaker: &mut CircuitBreaker, now: Instant) {
        for verdict in [Success, Success, Failure, Failure] {
            call(breaker, verdict, now);
        }
        assert_eq!(breaker.state(), CircuitState::Open);
    }

    #[test]
    fn 최소_호출_수를_채우기_전에는_열리지_않는다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        for _ in 0..3 {
            call(&mut breaker, Failure, t0);
        }
        assert_eq!(
            breaker.state(),
            CircuitState::Closed,
            "3회 실패는 아직 판단하지 않는다"
        );

        call(&mut breaker, Failure, t0);
        assert_eq!(breaker.state(), CircuitState::Open);
    }

    #[test]
    fn 실패율이_기준보다_낮으면_닫힌_채로_있다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        for verdict in [Success, Success, Success, Failure] {
            call(&mut breaker, verdict, t0);
        }
        assert_eq!(breaker.state(), CircuitState::Closed, "25% < 50%");
    }

    #[test]
    fn 창_크기만큼의_최근_결과만_본다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        // 실패 1 + 성공 3 → 25%. 그다음 성공이 들어오면 맨 앞의 실패가 창에서 빠진다.
        for verdict in [Failure, Success, Success, Success, Success, Failure] {
            call(&mut breaker, verdict, t0);
        }
        // 창: [성공, 성공, 성공, 실패] → 25%
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[test]
    fn 집계에서_빼는_결과는_세지_않는다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        for verdict in [
            Failure, Ignored, Ignored, Ignored, Ignored, Failure, Success,
        ] {
            call(&mut breaker, verdict, t0);
        }
        // 센 것은 실패 2 + 성공 1 = 3회뿐이라 아직 판단하지 않는다
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[test]
    fn 열려_있으면_정해진_시각까지_거절하고_그다음_반만_열린다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        open(&mut breaker, t0);

        let until = t0 + Duration::from_millis(1_000);
        assert_eq!(breaker.try_acquire(t0), Err(until));
        assert_eq!(
            breaker.try_acquire(t0 + Duration::from_millis(999)),
            Err(until)
        );

        assert!(breaker.try_acquire(until).is_ok());
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn 시험_호출이_모두_성공하면_닫힌다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        open(&mut breaker, t0);
        let later = t0 + Duration::from_secs(1);

        let first = breaker.try_acquire(later).unwrap();
        let second = breaker.try_acquire(later).unwrap();
        // 시험 자리(2개)가 다 나갔으므로 세 번째는 거절
        assert!(breaker.try_acquire(later).is_err());

        breaker.record(first, Success, later);
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
        breaker.record(second, Success, later);
        assert_eq!(breaker.state(), CircuitState::Closed);

        // 닫히면 지난 기록은 지우고 새로 센다
        call(&mut breaker, Failure, later);
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[test]
    fn 시험_호출이_하나라도_실패하면_다시_열린다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        open(&mut breaker, t0);
        let later = t0 + Duration::from_secs(1);

        let first = breaker.try_acquire(later).unwrap();
        let second = breaker.try_acquire(later).unwrap();
        breaker.record(first, Success, later);
        breaker.record(second, Failure, later);

        assert_eq!(breaker.state(), CircuitState::Open);
        assert_eq!(
            breaker.try_acquire(later),
            Err(later + Duration::from_secs(1)),
            "실패한 시각부터 다시 open_ms를 기다린다"
        );
    }

    #[test]
    fn 집계에서_빼는_시험_결과는_자리만_돌려준다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        open(&mut breaker, t0);
        let later = t0 + Duration::from_secs(1);

        let first = breaker.try_acquire(later).unwrap();
        let second = breaker.try_acquire(later).unwrap();
        breaker.record(first, Ignored, later);

        // 돌려받은 자리로 시험 호출을 하나 더 할 수 있다
        let third = breaker.try_acquire(later).unwrap();
        breaker.record(second, Success, later);
        breaker.record(third, Success, later);
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[test]
    fn 시험_호출_결과가_돌아오지_않으면_open_ms_뒤에_시험을_다시_시작한다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        open(&mut breaker, t0);
        let later = t0 + Duration::from_secs(1);

        // 시험 호출 두 개가 나갔는데 결과를 알리지 않고 사라졌다 (호출이 취소된 경우)
        let lost_first = breaker.try_acquire(later).unwrap();
        let lost_second = breaker.try_acquire(later).unwrap();
        assert!(
            breaker
                .try_acquire(later + Duration::from_millis(999))
                .is_err()
        );

        // open_ms가 지나면 시험 자리를 새로 연다
        let much_later = later + Duration::from_secs(1);
        let probe = breaker.try_acquire(much_later).unwrap();
        assert_eq!(breaker.state(), CircuitState::HalfOpen);

        // 사라졌던 호출의 늦은 결과는 새 시험에 섞이지 않는다
        breaker.record(lost_first, Failure, much_later);
        breaker.record(lost_second, Failure, much_later);
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
        breaker.record(probe, Success, much_later);
        let second_probe = breaker.try_acquire(much_later).unwrap();
        breaker.record(second_probe, Success, much_later);
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[test]
    fn 기다리는_동안_상태가_바뀌었으면_허락을_다시_구한다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        let waiting = breaker.try_acquire(t0).unwrap();
        assert_eq!(
            breaker.revalidate(waiting, t0),
            Ok(waiting),
            "그대로면 같은 허락"
        );

        // 기다리는 동안 다른 호출들의 실패로 열렸다 → 지금은 호출하면 안 된다
        open(&mut breaker, t0);
        assert_eq!(
            breaker.revalidate(waiting, t0),
            Err(t0 + Duration::from_secs(1))
        );

        // 반만 열린 상태로 바뀐 뒤라면 시험 자리를 받아 간다
        let later = t0 + Duration::from_secs(1);
        let probe = breaker.revalidate(waiting, later).unwrap();
        assert_ne!(probe, waiting);
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn 강제로_열면_open_ms가_지나도_풀_때까지_막는다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        breaker.control(CircuitAction::ForceOpen, t0);
        assert_eq!(breaker.state(), CircuitState::Open);

        let much_later = t0 + Duration::from_secs(60);
        assert_eq!(
            breaker.try_acquire(much_later),
            Err(much_later + Duration::from_secs(1)),
            "시험 호출로 넘어가지 않고 open_ms 뒤에 다시 물으라고 한다"
        );
        assert_eq!(breaker.state(), CircuitState::Open);
        let snapshot = breaker.snapshot();
        assert_eq!(snapshot.forced, Some(ForcedState::Open));
        assert_eq!(snapshot.open_until, None, "강제로 열면 끝나는 시각이 없다");

        breaker.control(CircuitAction::Reset, much_later);
        assert_eq!(breaker.state(), CircuitState::Closed);
        assert!(breaker.try_acquire(much_later).is_ok());
        assert_eq!(breaker.snapshot().forced, None);
    }

    #[test]
    fn 강제로_닫으면_실패가_쌓여도_열리지_않는다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        open(&mut breaker, t0);

        breaker.control(CircuitAction::ForceClose, t0);
        assert_eq!(breaker.state(), CircuitState::Closed);
        for _ in 0..10 {
            call(&mut breaker, Failure, t0);
        }
        assert_eq!(breaker.state(), CircuitState::Closed);
        let snapshot = breaker.snapshot();
        assert_eq!(snapshot.forced, Some(ForcedState::Closed));
        assert_eq!(snapshot.recent_calls, 0, "강제된 동안의 결과는 세지 않는다");

        // 풀면 처음부터 다시 센다
        breaker.control(CircuitAction::Reset, t0);
        open(&mut breaker, t0);
    }

    #[test]
    fn 조작_전에_시작한_호출의_결과는_세지_않는다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        let before = breaker.try_acquire(t0).unwrap();
        breaker.control(CircuitAction::Reset, t0);
        breaker.record(before, Failure, t0);
        assert_eq!(breaker.snapshot().recent_calls, 0);

        // 강제로 열기 전에 허락받고 기다리던 호출은 다시 확인할 때 막힌다
        let waiting = breaker.try_acquire(t0).unwrap();
        breaker.control(CircuitAction::ForceOpen, t0);
        assert!(breaker.revalidate(waiting, t0).is_err());
    }

    #[test]
    fn 스냅샷은_상태별_진행_상황을_보여준다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        for verdict in [Success, Failure, Ignored] {
            call(&mut breaker, verdict, t0);
        }
        let closed = breaker.snapshot();
        assert_eq!(
            (closed.recent_calls, closed.recent_failures),
            (2, 1),
            "집계에서 빼는 결과는 세지 않는다"
        );

        let mut breaker = CircuitBreaker::new(policy());
        open(&mut breaker, t0);
        assert_eq!(
            breaker.snapshot().open_until,
            Some(t0 + Duration::from_secs(1))
        );

        let later = t0 + Duration::from_secs(1);
        let probe = breaker.try_acquire(later).unwrap();
        let _second = breaker.try_acquire(later).unwrap();
        breaker.record(probe, Success, later);
        let half_open = breaker.snapshot();
        assert_eq!(half_open.state, CircuitState::HalfOpen);
        assert_eq!(
            (half_open.probes_succeeded, half_open.probes_in_flight),
            (1, 1)
        );
    }

    #[test]
    fn 정책을_바꾸면_상태는_그대로_두고_다음_판단부터_새_기준을_쓴다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        for verdict in [Success, Success, Failure] {
            call(&mut breaker, verdict, t0);
        }
        // 창을 2로 줄이면 오래된 결과(성공 하나)를 버린다: [성공, 실패]
        breaker.update_policy(CircuitBreakerPolicy {
            window: 2,
            min_calls: 2,
            ..policy()
        });
        let snapshot = breaker.snapshot();
        assert_eq!((snapshot.recent_calls, snapshot.recent_failures), (2, 1));
        assert_eq!(
            breaker.state(),
            CircuitState::Closed,
            "바꾸는 것만으로는 열리지 않는다"
        );

        // 다음 결과부터 새 기준(최근 2회 중 50%)으로 판단한다
        call(&mut breaker, Failure, t0);
        assert_eq!(breaker.state(), CircuitState::Open);

        // 열린 서킷은 정책을 바꿔도 열린 채로 있다
        breaker.update_policy(policy());
        assert_eq!(breaker.state(), CircuitState::Open);
    }

    #[test]
    fn 다른_상태일_때_시작한_호출의_늦은_결과는_무시한다() {
        let mut breaker = CircuitBreaker::new(policy());
        let t0 = Instant::now();
        // Closed일 때 시작한 호출 (아직 결과가 오지 않음)
        let slow = breaker.try_acquire(t0).unwrap();
        open(&mut breaker, t0);

        let later = t0 + Duration::from_secs(1);
        let probe = breaker.try_acquire(later).unwrap();
        assert_eq!(breaker.state(), CircuitState::HalfOpen);

        // 늦게 도착한 Closed 시절 결과. 시험 호출 성공으로 세면 안 된다.
        breaker.record(slow, Success, later);
        breaker.record(probe, Success, later);
        assert_eq!(
            breaker.state(),
            CircuitState::HalfOpen,
            "시험 호출 성공은 1회뿐이라 아직 닫히지 않는다"
        );
    }
}
