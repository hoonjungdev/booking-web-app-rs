//! 장애 대응 파이프라인 (docs/03-resilience.md §2)
//!
//! ```text
//! ResilientProvider::call(operation)
//! ├─ [전체 시간 예산]      호출 1회당 최대 call_budget_ms
//! └─ [재시도 루프]         최대 max_attempts회, 지수 백오프 + full jitter
//!     └─ 시도할 때마다:
//!         ├─ [업체 pause 확인] Retry-After로 쉬는 중이면 바로 Throttled
//!         ├─ [서킷 브레이커]   열려 있으면 바로 CircuitOpen
//!         ├─ [rate limiter]    토큰을 기다림 (최대 max_wait_ms, 넘으면 Throttled)
//!         ├─ [bulkhead]        업체별 동시 실행 수 제한 (최대 max_wait_ms 대기, 넘으면 Throttled)
//!         ├─ [시도 타임아웃]   attempt_timeout_ms (남은 예산이 더 짧으면 그만큼)
//!         ├─ 어댑터 호출 → 결과 분류
//!         └─ 기록: 서킷 집계, 429면 업체 pause, provider_calls 기록, 지표
//! ```
//!
//! 추적(trace)에서는 호출 하나가 `provider.call` span, 그 안의 시도 하나하나가 `provider.attempt` span이 된다.
//! 재시도가 있었는지, 어느 시도가 얼마나 걸렸는지, 관문에서 막혔는지가 trace 한 장에 보인다.
//!
//! Polly로 `HttpClient`를 감싸는 것과 같은 구조다. 어댑터는 HTTP ↔ 도메인 변환과 응답 분류만 하고,
//! 재시도 · 서킷 · rate limit은 모두 여기서 한다.

mod circuit;
mod retry;
#[cfg(test)]
mod tests;

use std::{
    future::Future,
    num::NonZeroU32,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use domain::{BookingId, ProviderId};
use governor::{
    Quota, RateLimiter,
    clock::MonotonicClock,
    middleware::StateInformationMiddleware,
    state::{InMemoryState, NotKeyed},
};
use rand::RngExt;
use tokio::sync::{Semaphore, SemaphorePermit};
use tracing::{Span, field::Empty};

pub use circuit::{
    CircuitAction, CircuitBreaker, CircuitSnapshot, CircuitState, ForcedState, Permit,
};
pub use retry::{MAX_IN_CALL_RETRY_AFTER, backoff_delay, may_retry};

use crate::{
    AdapterReply, CallRecord, CallRecorder, Capabilities, CircuitVerdict, CreateBooking,
    ProviderAdapter, ProviderBooking, ProviderError, ProviderPolicy, ResponseMeta,
    metrics::{self, ThrottleSource},
    trace,
};

/// 업체가 `Retry-After` 없이 호출 한도 초과를 알렸을 때 쉬는 시간
const DEFAULT_PAUSE: Duration = Duration::from_secs(1);

/// 업체 하나의 rate limiter. 키 없이(direct) 하나의 버킷을 쓰고, 시계는 표준 라이브러리의 단조 시계다.
/// `StateInformationMiddleware`: 토큰을 받을 때마다 "버킷에 몇 개 남았는지"도 함께 알려 준다 (관리자 화면의 limiter 여유).
type Limiter = RateLimiter<NotKeyed, InMemoryState, MonotonicClock, StateInformationMiddleware>;

/// 업체 파이프라인의 지금 상태 (관리자 화면의 업체 런타임 상태). 모두 이 프로세스 메모리의 값이다 (docs/decisions.md D-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub circuit: CircuitSnapshot,
    /// 업체가 쉬라고 해서(`Retry-After`) 호출을 멈춘 상태면 그 끝 시각
    pub paused_until: Option<Instant>,
    /// 지금 업체를 부르고 있는 호출 수와 bulkhead의 전체 자리 수
    pub in_flight: usize,
    pub max_concurrency: usize,
    /// 우리 rate limiter 버킷에 남은 토큰 수 (추정값)와 버킷 크기
    pub limiter_available: u32,
    pub limiter_burst: u32,
    /// 업체가 마지막 응답에서 알려 준 남은 호출 수 (`X-RateLimit-Remaining`)
    pub rate_limit_remaining: Option<u32>,
}

/// 업체 호출 종류. 호출 기록의 `operation` 값이 된다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    CreateBooking,
    FindBooking,
}

impl Operation {
    /// 모든 호출 종류. 종류를 추가하면 여기에도 넣는다.
    pub const ALL: [Self; 2] = [Self::CreateBooking, Self::FindBooking];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CreateBooking => "create_booking",
            Self::FindBooking => "find_booking",
        }
    }

    /// 업체 쪽 상태를 바꾸지 않는 조회 호출인가. 조회는 몇 번을 다시 보내도 안전하다.
    pub fn is_read_only(self) -> bool {
        match self {
            Self::CreateBooking => false,
            Self::FindBooking => true,
        }
    }
}

/// 호출 기록에 함께 남길 정보 (어떤 예약 · 작업 때문에 부른 호출인가)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallContext {
    pub booking_id: Option<BookingId>,
    pub job_id: Option<i64>,
}

/// 어댑터를 감싸 재시도 · 타임아웃 · 서킷 브레이커 · rate limit · bulkhead를 적용한다.
///
/// 업체마다 하나씩 만들어 `Arc`로 공유한다. 서킷 상태와 rate limit 버킷은 이 객체 안(프로세스 메모리)에 있다
/// (docs/decisions.md D-10).
pub struct ResilientProvider {
    id: ProviderId,
    adapter: Arc<dyn ProviderAdapter>,
    policy: ProviderPolicy,
    /// 여러 워커 태스크가 함께 쓰므로 잠금으로 감싼다. 잠금은 상태를 읽고 바꾸는 짧은 순간에만 쥐고,
    /// `await` 중에는 쥐지 않는다 (std `Mutex`를 쥔 채 기다리면 다른 태스크가 멈춘다).
    breaker: Mutex<CircuitBreaker>,
    limiter: Limiter,
    /// limiter에 마지막으로 물었을 때 남아 있던 토큰 수와 그 시각. governor는 토큰을 쓰지 않고 남은 수만
    /// 엿보는 기능이 없어서, 이 값에 그 뒤로 다시 채워졌을 토큰을 더해 지금 남은 수를 추정한다.
    limiter_seen: Mutex<Option<(Instant, u32)>>,
    /// 동시 호출 수 제한 (≈ C#의 `SemaphoreSlim`)
    bulkhead: Semaphore,
    /// bulkhead의 전체 자리 수
    max_concurrency: usize,
    /// 업체가 쉬라고 한 시각까지 (`Retry-After`)
    paused_until: Mutex<Option<Instant>>,
    /// 업체가 마지막으로 알려 준 남은 호출 수 (`X-RateLimit-Remaining`)
    rate_limit_remaining: Mutex<Option<u32>>,
    recorder: CallRecorder,
}

impl ResilientProvider {
    /// `policy`는 [`ProviderPolicy::validate`]를 통과한 값이어야 한다 (0인 값은 1로 올려 쓴다).
    pub fn new(
        id: ProviderId,
        adapter: Arc<dyn ProviderAdapter>,
        policy: ProviderPolicy,
        recorder: CallRecorder,
    ) -> Self {
        // NonZeroU32: 0이 될 수 없는 정수 타입. governor는 "초당 0회" 같은 말이 안 되는 설정을 타입으로 막는다.
        let per_second = NonZeroU32::new(policy.rate_limit.per_second).unwrap_or(NonZeroU32::MIN);
        let burst = NonZeroU32::new(policy.rate_limit.burst).unwrap_or(NonZeroU32::MIN);
        let quota = Quota::per_second(per_second).allow_burst(burst);
        let max_concurrency = policy.bulkhead.max_concurrency.max(1) as usize;
        metrics::register_provider(&id);
        Self {
            breaker: Mutex::new(CircuitBreaker::new(policy.circuit_breaker)),
            limiter: RateLimiter::direct_with_clock(quota, MonotonicClock)
                .with_middleware::<StateInformationMiddleware>(),
            limiter_seen: Mutex::new(None),
            bulkhead: Semaphore::new(max_concurrency),
            max_concurrency,
            paused_until: Mutex::new(None),
            rate_limit_remaining: Mutex::new(None),
            id,
            adapter,
            policy,
            recorder,
        }
    }

    pub fn id(&self) -> &ProviderId {
        &self.id
    }

    pub fn capabilities(&self) -> Capabilities {
        self.adapter.capabilities()
    }

    pub fn circuit_state(&self) -> CircuitState {
        lock(&self.breaker).state()
    }

    /// 호출 한 번이 걸릴 수 있는 최대 시간: 호출 예산 + 마지막 시도의 관문 대기(rate limiter + bulkhead).
    /// 워커의 lease는 이보다 길어야 한다. 짧으면 호출 도중 lease가 끝나 다른 워커가 같은 작업을 다시 가져간다.
    pub fn longest_call(&self) -> Duration {
        self.policy.call_budget()
            + self.policy.rate_limit.max_wait()
            + self.policy.bulkhead.max_wait()
    }

    /// 업체가 쉬라고 해서 호출을 멈춘 상태면 그 끝 시각
    pub fn paused_until(&self) -> Option<Instant> {
        let now = Instant::now();
        lock(&self.paused_until).filter(|until| *until > now)
    }

    /// 지금 업체를 부르고 있는 호출 수 (bulkhead에서 나가 있는 자리 수)
    pub fn in_flight(&self) -> usize {
        self.max_concurrency
            .saturating_sub(self.bulkhead.available_permits())
    }

    /// 업체가 마지막 응답에서 알려 준 남은 호출 수. 아직 받은 적이 없으면 `None`.
    pub fn rate_limit_remaining(&self) -> Option<u32> {
        *lock(&self.rate_limit_remaining)
    }

    /// 이 업체에 적용 중인 정책 (서버가 시작할 때 DB에서 읽은 값)
    pub fn policy(&self) -> &ProviderPolicy {
        &self.policy
    }

    /// 서킷 상태를 자세히 본다
    pub fn circuit(&self) -> CircuitSnapshot {
        lock(&self.breaker).snapshot()
    }

    /// 관리자가 서킷을 강제로 열거나 닫거나, 강제 상태를 푼다
    pub fn control_circuit(&self, action: CircuitAction) {
        self.with_breaker(|breaker| breaker.control(action, Instant::now()));
        tracing::info!(provider = %self.id, ?action, "관리자가 서킷을 조작했다");
    }

    /// 우리 rate limiter 버킷에 지금 남은 토큰 수 (추정값). 아직 한 번도 묻지 않았으면 버킷이 가득 차 있다.
    ///
    /// 마지막으로 물었을 때 남은 수 + 그 뒤로 다시 채워졌을 수 (초당 `per_second`개), 단 버킷 크기를 넘지 않는다.
    pub fn limiter_available(&self) -> u32 {
        let burst = self.policy.rate_limit.burst.max(1);
        let Some((seen_at, remaining)) = *lock(&self.limiter_seen) else {
            return burst;
        };
        let refilled =
            seen_at.elapsed().as_secs_f64() * f64::from(self.policy.rate_limit.per_second);
        // `as u32`: 실수를 정수로 바꿀 때 소수점 아래는 버리고, 범위를 넘으면 u32의 최댓값으로 맞춘다
        remaining.saturating_add(refilled as u32).min(burst)
    }

    /// 관리자 화면에 보여 줄 지금 상태를 한 번에 모은다
    pub fn runtime_status(&self) -> RuntimeStatus {
        RuntimeStatus {
            circuit: self.circuit(),
            paused_until: self.paused_until(),
            in_flight: self.in_flight(),
            max_concurrency: self.max_concurrency,
            limiter_available: self.limiter_available(),
            limiter_burst: self.policy.rate_limit.burst.max(1),
            rate_limit_remaining: self.rate_limit_remaining(),
        }
    }

    /// 업체에 예약을 만든다
    pub async fn create_booking(
        &self,
        request: &CreateBooking,
        ctx: CallContext,
    ) -> Result<ProviderBooking, ProviderError> {
        let adapter = self.adapter.as_ref();
        // `move || ...`: 참조 두 개(adapter, request)를 복사해 담은 클로저. 시도할 때마다 불러서 새 요청을 만든다.
        self.call(Operation::CreateBooking, ctx, move || {
            adapter.create_booking(request)
        })
        .await
    }

    /// 우리 참조값(예약 ID)으로 업체 예약을 찾는다. 없으면 `Ok(None)`.
    pub async fn find_booking(
        &self,
        reference: BookingId,
        ctx: CallContext,
    ) -> Result<Option<ProviderBooking>, ProviderError> {
        let adapter = self.adapter.as_ref();
        self.call(Operation::FindBooking, ctx, move || {
            adapter.find_booking(reference)
        })
        .await
    }

    /// 모든 호출이 지나가는 입구. `provider.call` span을 열고 재시도 루프를 돌린다.
    ///
    /// `#[tracing::instrument]`: 이 함수가 실행되는 동안 span을 연다. `skip_all`은 "인자를 span에 자동으로 기록하지 않는다"는
    /// 뜻이다 (인자에 고객 정보가 들어 있을 수 있다). 남길 값은 `fields(..)`에 직접 고른다.
    /// `Option` 값은 `Some`일 때만 기록된다.
    #[tracing::instrument(
        name = "provider.call",
        skip_all,
        fields(
            provider = %self.id,
            operation = operation.as_str(),
            booking_id = ctx.booking_id.map(tracing::field::display),
            job_id = ctx.job_id,
            otel.status_code = Empty,
        )
    )]
    async fn call<T, F, Fut>(
        &self,
        operation: Operation,
        ctx: CallContext,
        send: F,
    ) -> Result<T, ProviderError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = AdapterReply<T>>,
    {
        let result = self.retry_loop(operation, ctx, send).await;
        if result.as_ref().is_err_and(is_failure) {
            Span::current().record("otel.status_code", "error");
        }
        result
    }

    /// 재시도 루프.
    ///
    /// 제네릭 함수다 (≈ C#의 `Task<T> CallAsync<T>(Func<Task<Reply<T>>> send)`).
    /// - `T`: 호출이 성공하면 돌려줄 값의 타입 (예약 생성은 `ProviderBooking`, 조회는 `Option<ProviderBooking>`)
    /// - `F`: 시도 한 번을 시작하는 클로저. 재시도마다 다시 부르므로 `FnMut`(여러 번 부를 수 있는 클로저)
    /// - `Fut`: 그 클로저가 돌려주는 Future. `async fn`의 반환값처럼 `.await`하면 결과가 나온다
    ///
    /// 업체를 부르지 않은 실패(`CircuitOpen`, `Throttled`)와 업체가 처리했을지 모르는 실패를 섞지 않는다:
    /// 처리 여부를 모르는 실패 뒤에는 멱등 안전할 때만 다시 보내므로, 멱등키가 없는 생성 호출이 이중으로 나가지 않는다.
    ///
    /// 재시도가 관문에서 막혔는데 앞선 시도가 업체까지 갔다면, 막힌 실패 대신 **앞선 시도의 실패**를 돌려준다.
    /// 워커는 `CircuitOpen` · `Throttled`를 "부르지 않았다"로 보고 시도 횟수를 되돌리는데, 실제로 부른 호출까지
    /// 되돌리면 업체가 계속 죽어 있을 때 작업이 끝없이 미뤄지기 때문이다 (docs/03-resilience.md §4).
    async fn retry_loop<T, F, Fut>(
        &self,
        operation: Operation,
        ctx: CallContext,
        mut send: F,
    ) -> Result<T, ProviderError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = AdapterReply<T>>,
    {
        let deadline = Instant::now() + self.policy.call_budget();
        let idempotent_safe =
            operation.is_read_only() || self.adapter.capabilities().idempotency_key;
        let max_attempts = self.policy.retry.max_attempts.max(1);

        let mut attempt = 1;
        // 업체까지 간 시도 중 마지막 실패 (재시도한 것만 담긴다)
        let mut last_called: Option<ProviderError> = None;
        loop {
            let error = match self
                .attempt(operation, ctx, attempt, deadline, &mut send)
                .await
            {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            if error.blocked_until().is_some() {
                // 이번 시도는 관문에서 막혔다. 앞선 시도가 업체까지 갔으면 그 실패를 돌려준다.
                return Err(last_called.unwrap_or(error));
            }

            let Some(floor) = may_retry(&error, idempotent_safe) else {
                return Err(error);
            };
            if attempt >= max_attempts {
                return Err(error);
            }
            let backoff = backoff_delay(
                attempt,
                self.policy.retry.base_delay(),
                self.policy.retry.max_delay(),
                rand::rng().random(),
            );
            let wait = backoff.max(floor);
            // 다음 시도를 시도 타임아웃만큼 온전히 할 시간이 없으면 시작하지 않는다.
            // 남은 몇 ms로 보낸 요청은 거의 타임아웃이 나고, 멱등키 없는 업체에서는 괜히 결과 불명이 된다.
            if Instant::now() + wait + self.policy.attempt_timeout() > deadline {
                tracing::debug!(attempt, "호출 예산 안에 다음 시도를 할 수 없어 멈춘다");
                return Err(error);
            }
            tracing::debug!(
                attempt,
                outcome = error.outcome(),
                wait_ms = wait.as_millis() as u64,
                "같은 호출 안에서 다시 시도한다"
            );
            last_called = Some(error);
            metrics::retried(&self.id, operation.as_str());
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    /// 시도 한 번: 관문을 통과하면 어댑터를 부르고, 결과를 서킷 · pause · 호출 기록에 반영한다.
    ///
    /// span의 `outcome`은 끝나야 알 수 있으므로 `Empty`로 자리만 잡아 두고 `record`에서 채운다.
    #[tracing::instrument(
        name = "provider.attempt",
        skip_all,
        // `attempt = attempt`처럼 값을 적어야 한다. 이름만 쓰면(`fields(attempt)`) 값이 빈 필드가 된다.
        fields(attempt = attempt, outcome = Empty, otel.status_code = Empty)
    )]
    async fn attempt<T, F, Fut>(
        &self,
        operation: Operation,
        ctx: CallContext,
        attempt: u32,
        deadline: Instant,
        send: &mut F,
    ) -> Result<T, ProviderError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = AdapterReply<T>>,
    {
        let info = AttemptInfo {
            operation,
            ctx,
            attempt,
            started_at: Utc::now(),
        };
        let waiting_since = Instant::now();

        // 관문에서 막히면 업체를 부르지 않는다. 막힌 것도 기록해 둔다 (outcome: circuit_open / throttled).
        let (permit, slot) = match self.admit(deadline).await {
            Ok(admitted) => admitted,
            Err(error) => {
                self.record(
                    &info,
                    waiting_since.elapsed(),
                    Some(&error),
                    ResponseMeta::default(),
                );
                return Err(error);
            }
        };

        let timeout = self
            .policy
            .attempt_timeout()
            .min(deadline.saturating_duration_since(Instant::now()));
        let called_at = Instant::now();
        // tokio::time::timeout: 정해진 시간 안에 끝나지 않으면 Future를 버리고 Err를 돌려준다
        // (≈ C#의 `task.WaitAsync(timeout)` + 취소). 버려진 HTTP 요청은 연결이 끊긴다.
        let reply = match tokio::time::timeout(timeout, send()).await {
            Ok(reply) => reply,
            Err(_elapsed) => AdapterReply::from(Err(ProviderError::Timeout)),
        };
        let duration = called_at.elapsed();

        let verdict = match &reply.result {
            Ok(_) => CircuitVerdict::Success,
            Err(error) => error.circuit_verdict(),
        };
        self.with_breaker(|breaker| breaker.record(permit, verdict, Instant::now()));
        if let Err(ProviderError::RateLimited { retry_after }) = &reply.result {
            self.pause(retry_after.unwrap_or(DEFAULT_PAUSE));
        }
        // 서킷 · pause에 결과를 반영한 **뒤에** bulkhead 자리를 반납한다. 자리를 기다리던 다음 호출이
        // 이 결과가 반영된 상태(예: 방금 열린 서킷)를 보고 판단하게 하려는 것이다.
        drop(slot);
        self.record(&info, duration, reply.result.as_ref().err(), reply.meta);
        reply.result
    }

    /// 업체를 부르기 전 관문: pause → 서킷 → rate limiter → bulkhead → 남은 예산.
    ///
    /// 통과하면 서킷 허락 증표와 bulkhead 자리를 돌려준다. 자리(`SemaphorePermit`)는 drop될 때 자동으로 반납된다
    /// (≈ C#의 `using`). `SemaphorePermit<'_>`의 `'_`는 "이 자리는 `self`의 semaphore를 빌린 것"이라는 표시다.
    async fn admit(
        &self,
        deadline: Instant,
    ) -> Result<(Permit, SemaphorePermit<'_>), ProviderError> {
        if let Some(until) = self.paused_until() {
            return Err(ProviderError::Throttled { retry_at: until });
        }
        let permit = self
            .with_breaker(|breaker| breaker.try_acquire(Instant::now()))
            .map_err(|retry_at| ProviderError::CircuitOpen { retry_at })?;

        let slot = async {
            self.wait_for_token().await?;
            let slot = self.acquire_bulkhead().await?;
            if Instant::now() >= deadline {
                // 기다리는 동안 호출 예산을 다 썼다. 부르지 않고 돌려보낸다.
                return Err(ProviderError::Throttled {
                    retry_at: Instant::now(),
                });
            }
            Ok(slot)
        }
        .await;

        let slot = match slot {
            Ok(slot) => slot,
            Err(error) => {
                // 서킷에서 허락은 받았지만 호출하지 않았다. 집계하지 않고 허락만 돌려준다 (시험 호출 자리 반납).
                self.with_breaker(|breaker| {
                    breaker.record(permit, CircuitVerdict::Ignored, Instant::now());
                });
                return Err(error);
            }
        };
        // 기다리는 동안(최대 rate limit + bulkhead 대기) 서킷이 열렸을 수 있다. 지금 상태로 허락을 다시 확인한다.
        // 막히면 `?`가 Err를 돌려주고, 그때 slot도 drop되어 반납된다.
        let permit = self
            .with_breaker(|breaker| breaker.revalidate(permit, Instant::now()))
            .map_err(|retry_at| ProviderError::CircuitOpen { retry_at })?;
        Ok((permit, slot))
    }

    /// rate limiter 토큰을 기다린다. `max_wait_ms` 안에 받을 수 없으면 바로 `Throttled`.
    async fn wait_for_token(&self) -> Result<(), ProviderError> {
        let give_up_at = Instant::now() + self.policy.rate_limit.max_wait();
        loop {
            // check(): 토큰이 있으면 하나 쓰고 Ok(남은 상태), 없으면 언제 생기는지 알려 주는 Err
            let not_until = match self.limiter.check() {
                Ok(snapshot) => {
                    self.saw_limiter(snapshot.remaining_burst_capacity());
                    return Ok(());
                }
                Err(not_until) => {
                    self.saw_limiter(0);
                    not_until
                }
            };
            let now = Instant::now();
            let ready_at = now + not_until.wait_time_from(now);
            if ready_at > give_up_at {
                return Err(ProviderError::Throttled { retry_at: ready_at });
            }
            tokio::time::sleep(ready_at - now).await;
        }
    }

    /// limiter 버킷에 남은 토큰 수를 기억해 둔다 ([`Self::limiter_available`])
    fn saw_limiter(&self, remaining: u32) {
        *lock(&self.limiter_seen) = Some((Instant::now(), remaining));
    }

    /// bulkhead 자리를 기다린다. `max_wait_ms` 안에 자리가 나지 않으면 `Throttled`.
    async fn acquire_bulkhead(&self) -> Result<SemaphorePermit<'_>, ProviderError> {
        let max_wait = self.policy.bulkhead.max_wait();
        match tokio::time::timeout(max_wait, self.bulkhead.acquire()).await {
            Ok(Ok(slot)) => Ok(slot),
            // Err(Elapsed): 기다려도 자리가 나지 않음. Ok(Err(_)): semaphore가 닫힘 (닫는 곳이 없어 일어나지 않음)
            Ok(Err(_)) | Err(_) => Err(ProviderError::Throttled {
                retry_at: Instant::now() + max_wait,
            }),
        }
    }

    /// 업체가 쉬라고 한 만큼 이 업체 호출을 모두 멈춘다 (이미 더 길게 멈춰 있으면 그대로 둔다)
    fn pause(&self, duration: Duration) {
        let until = Instant::now() + duration;
        let extended = {
            let mut paused = lock(&self.paused_until);
            let extend = paused.is_none_or(|current| current < until);
            if extend {
                *paused = Some(until);
            }
            extend
        };
        if extended {
            tracing::warn!(
                provider = %self.id,
                pause_ms = duration.as_millis() as u64,
                "업체가 호출 한도 초과를 알려 잠시 호출을 멈춘다"
            );
        }
    }

    /// 서킷 브레이커를 잠그고 `action`을 실행한다. 상태가 바뀌었으면 로그를 남긴다.
    ///
    /// `impl FnOnce(&mut CircuitBreaker) -> R`: 브레이커를 받아 무언가를 하고 `R`을 돌려주는 클로저 (≈ `Func<CircuitBreaker, R>`)
    fn with_breaker<R>(&self, action: impl FnOnce(&mut CircuitBreaker) -> R) -> R {
        let (result, before, after) = {
            let mut breaker = lock(&self.breaker);
            let before = breaker.state();
            let result = action(&mut breaker);
            (result, before, breaker.state())
        };
        if before != after {
            if after == CircuitState::Open {
                tracing::warn!(provider = %self.id, from = before.as_str(), to = after.as_str(), "circuit opened");
            } else {
                tracing::info!(provider = %self.id, from = before.as_str(), to = after.as_str(), "circuit state changed");
            }
        }
        result
    }

    fn record(
        &self,
        info: &AttemptInfo,
        duration: Duration,
        error: Option<&ProviderError>,
        meta: ResponseMeta,
    ) {
        let outcome = error.map_or("success", ProviderError::outcome);
        tracing::debug!(
            attempt = info.attempt,
            outcome,
            duration_ms = duration.as_millis() as u64,
            "provider attempt finished"
        );
        // 지금 span은 이 시도의 `provider.attempt`다 (이 함수는 `attempt` 안에서만 불린다)
        let span = Span::current();
        span.record("outcome", outcome);
        if error.is_some_and(is_failure) {
            span.record("otel.status_code", "error");
        }

        metrics::attempt_finished(&self.id, info.operation.as_str(), outcome, duration);
        match error {
            Some(ProviderError::RateLimited { .. }) => {
                metrics::throttled(&self.id, ThrottleSource::Remote);
            }
            Some(ProviderError::Throttled { .. }) => {
                metrics::throttled(&self.id, ThrottleSource::Local);
            }
            _ => {}
        }
        if let Some(remaining) = meta.rate_limit_remaining {
            *lock(&self.rate_limit_remaining) = Some(remaining);
        }

        let retry_after = match error {
            Some(ProviderError::RateLimited { retry_after }) => *retry_after,
            _ => None,
        };
        self.recorder.record(CallRecord {
            provider_id: self.id.clone(),
            operation: info.operation.as_str(),
            booking_id: info.ctx.booking_id,
            job_id: info.ctx.job_id,
            attempt: info.attempt,
            started_at: info.started_at,
            duration,
            outcome,
            http_status: meta.http_status,
            rate_limit_remaining: meta.rate_limit_remaining,
            retry_after,
            // 호출 기록에만 업체가 보낸 글자(4xx 본문 등)를 남긴다. 로그 · span에는 메시지만 나간다.
            error_detail: error.map(ProviderError::detail),
            trace_id: trace::current_trace_id(),
        });
    }
}

/// trace에서 "실패"로 표시할 결과인가. 업무적 거절(슬롯 마감 등)은 업체가 정상으로 답한 것이라 실패로 치지 않는다.
fn is_failure(error: &ProviderError) -> bool {
    !matches!(error, ProviderError::Rejected { .. })
}

/// 시도 하나를 기록할 때 쓰는 정보
struct AttemptInfo {
    operation: Operation,
    ctx: CallContext,
    attempt: u32,
    started_at: DateTime<Utc>,
}

/// `Mutex`를 잠근다. 잠금을 쥔 스레드가 패닉해 오염(poisoned)되어도 안의 값을 그대로 쓴다.
/// 서킷 상태 같은 값은 패닉 한 번으로 못 쓰게 되는 것보다 계속 쓰는 편이 낫다.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
