//! `ResilientProvider` 테스트: 정해 둔 응답을 돌려주는 가짜 어댑터를 감싸 관문과 재시도 규칙을 확인한다.
//!
//! 실제 시간으로 돌리므로 지연 · 타임아웃은 수십~수백 ms로 짧게 잡는다.

use std::{
    collections::VecDeque,
    sync::atomic::{AtomicUsize, Ordering},
};

use tokio::sync::mpsc;

use super::*;
use crate::{RateLimitPolicy, call_recorder};

const MS: Duration = Duration::from_millis(1);

const IDEMPOTENT: Capabilities = Capabilities {
    idempotency_key: true,
    lookup_by_reference: true,
    list_bookings: true,
};
const NOT_IDEMPOTENT: Capabilities = Capabilities {
    idempotency_key: false,
    lookup_by_reference: true,
    list_bookings: false,
};

type Step<T> = (Duration, AdapterReply<T>);

/// 정해 둔 응답을 순서대로 돌려주는 가짜 어댑터. 응답마다 지연을 줄 수 있다.
struct Scripted {
    capabilities: Capabilities,
    creates: Mutex<VecDeque<Step<ProviderBooking>>>,
    finds: Mutex<VecDeque<Step<Option<ProviderBooking>>>>,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            creates: Mutex::new(VecDeque::new()),
            finds: Mutex::new(VecDeque::new()),
            calls: AtomicUsize::new(0),
        }
    }

    fn create(self, delay: Duration, result: Result<ProviderBooking, ProviderError>) -> Self {
        lock(&self.creates).push_back((delay, result.into()));
        self
    }

    fn find(self, result: Result<Option<ProviderBooking>, ProviderError>) -> Self {
        lock(&self.finds).push_back((Duration::ZERO, result.into()));
        self
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// 준비한 응답이 떨어지면 돌려주는 값 (테스트가 예상보다 많이 불렀다는 신호)
fn exhausted<T>() -> Step<T> {
    (
        Duration::ZERO,
        Err(ProviderError::Server { status: 599 }).into(),
    )
}

#[async_trait::async_trait]
impl ProviderAdapter for Scripted {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    async fn create_booking(&self, _request: &CreateBooking) -> AdapterReply<ProviderBooking> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (delay, reply) = lock(&self.creates).pop_front().unwrap_or_else(exhausted);
        tokio::time::sleep(delay).await;
        reply
    }

    async fn find_booking(&self, _reference: BookingId) -> AdapterReply<Option<ProviderBooking>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (delay, reply) = lock(&self.finds).pop_front().unwrap_or_else(exhausted);
        tokio::time::sleep(delay).await;
        reply
    }
}

fn booked(id: &str) -> Result<ProviderBooking, ProviderError> {
    Ok(ProviderBooking {
        provider_booking_id: id.into(),
    })
}

/// 테스트가 빨리 끝나도록 대기 시간을 줄이고, 서킷 · rate limit은 방해하지 않게 넉넉히 둔 정책
fn fast_policy() -> ProviderPolicy {
    ProviderPolicy {
        attempt_timeout_ms: 1_000,
        call_budget_ms: 5_000,
        retry: crate::RetryPolicy {
            max_attempts: 3,
            base_delay_ms: 1,
            max_delay_ms: 10,
        },
        rate_limit: RateLimitPolicy {
            per_second: 1_000,
            burst: 1_000,
            max_wait_ms: 100,
        },
        bulkhead: crate::BulkheadPolicy {
            max_concurrency: 8,
            max_wait_ms: 100,
        },
        circuit_breaker: crate::CircuitBreakerPolicy {
            window: 100,
            min_calls: 100,
            ..Default::default()
        },
    }
}

fn provider(
    adapter: Scripted,
    policy: ProviderPolicy,
) -> (ResilientProvider, Arc<Scripted>, mpsc::Receiver<CallRecord>) {
    let adapter = Arc::new(adapter);
    let (recorder, records) = call_recorder(100);
    let provider = ResilientProvider::new(
        ProviderId::parse("alpha").unwrap(),
        adapter.clone(),
        policy,
        recorder,
    );
    (provider, adapter, records)
}

fn request() -> CreateBooking {
    CreateBooking {
        reference: BookingId::from_uuid(uuid::Uuid::now_v7()),
        resource_id: "store-001".into(),
        slot_start: Utc::now(),
        party_size: 2,
        customer: domain::Customer {
            name: "홍길동".into(),
            phone: "010-0000-0000".into(),
        },
    }
}

/// 지금까지 쌓인 기록의 (시도 번호, outcome)
fn drain(records: &mut mpsc::Receiver<CallRecord>) -> Vec<(u32, &'static str)> {
    let mut seen = Vec::new();
    while let Ok(record) = records.try_recv() {
        seen.push((record.attempt, record.outcome));
    }
    seen
}

#[tokio::test]
async fn 연결_실패는_같은_호출_안에서_다시_시도한다() {
    let connect = || {
        Err(ProviderError::Connect {
            detail: "refused".into(),
        })
    };
    let adapter = Scripted::new(NOT_IDEMPOTENT)
        .create(MS, connect())
        .create(MS, connect())
        .create(MS, booked("rsv_1"));
    let (provider, adapter, mut records) = provider(adapter, fast_policy());

    let ctx = CallContext {
        booking_id: None,
        job_id: Some(7),
    };
    let result = provider.create_booking(&request(), ctx).await;

    assert_eq!(result, booked("rsv_1"));
    assert_eq!(adapter.calls(), 3);
    assert_eq!(
        drain(&mut records),
        vec![(1, "connect_error"), (2, "connect_error"), (3, "success")]
    );
}

#[tokio::test]
async fn 멱등키가_없으면_타임아웃_뒤에_생성_요청을_다시_보내지_않는다() {
    let adapter = Scripted::new(NOT_IDEMPOTENT)
        .create(MS, Err(ProviderError::Timeout))
        .create(MS, booked("rsv_dup"));
    let (provider, adapter, _records) = provider(adapter, fast_policy());

    let result = provider
        .create_booking(&request(), CallContext::default())
        .await;

    assert_eq!(result, Err(ProviderError::Timeout));
    assert_eq!(adapter.calls(), 1, "다시 보내면 이중 예약 위험");
}

#[tokio::test]
async fn 멱등키가_있으면_처리_여부를_모르는_실패도_다시_보낸다() {
    let adapter = Scripted::new(IDEMPOTENT)
        .create(MS, Err(ProviderError::Server { status: 503 }))
        .create(MS, Err(ProviderError::Timeout))
        .create(MS, booked("rsv_1"));
    let (provider, adapter, _records) = provider(adapter, fast_policy());

    let result = provider
        .create_booking(&request(), CallContext::default())
        .await;

    assert_eq!(result, booked("rsv_1"));
    assert_eq!(adapter.calls(), 3);
}

#[tokio::test]
async fn 조회는_멱등키가_없어도_다시_보낸다() {
    let adapter = Scripted::new(NOT_IDEMPOTENT)
        .find(Err(ProviderError::Server { status: 500 }))
        .find(Ok(None));
    let (provider, adapter, _records) = provider(adapter, fast_policy());

    let result = provider
        .find_booking(request().reference, CallContext::default())
        .await;

    assert_eq!(result, Ok(None));
    assert_eq!(adapter.calls(), 2);
}

#[tokio::test]
async fn 업무적_거절은_다시_보내지_않는다() {
    let adapter = Scripted::new(IDEMPOTENT).create(
        MS,
        Err(ProviderError::Rejected {
            reason: domain::RejectReason::SlotUnavailable,
        }),
    );
    let (provider, adapter, _records) = provider(adapter, fast_policy());

    let result = provider
        .create_booking(&request(), CallContext::default())
        .await;

    assert!(matches!(result, Err(ProviderError::Rejected { .. })));
    assert_eq!(adapter.calls(), 1);
}

#[tokio::test]
async fn 시도_타임아웃을_넘기면_timeout으로_분류한다() {
    let adapter = Scripted::new(NOT_IDEMPOTENT).create(500 * MS, booked("rsv_late"));
    let policy = ProviderPolicy {
        attempt_timeout_ms: 50,
        ..fast_policy()
    };
    let (provider, _adapter, mut records) = provider(adapter, policy);

    let started = Instant::now();
    let result = provider
        .create_booking(&request(), CallContext::default())
        .await;

    assert_eq!(result, Err(ProviderError::Timeout));
    assert!(
        started.elapsed() < 400 * MS,
        "응답을 기다리지 않고 끝나야 한다"
    );
    assert_eq!(drain(&mut records), vec![(1, "timeout")]);
}

#[tokio::test]
async fn 호출_예산_안에_다음_시도를_할_수_없으면_멈춘다() {
    // 업체가 300ms 뒤에 다시 오라고 했지만 예산은 100ms뿐이다
    let adapter = Scripted::new(IDEMPOTENT)
        .create(
            MS,
            Err(ProviderError::RateLimited {
                retry_after: Some(300 * MS),
            }),
        )
        .create(MS, booked("rsv_1"));
    let policy = ProviderPolicy {
        call_budget_ms: 100,
        ..fast_policy()
    };
    let (provider, adapter, _records) = provider(adapter, policy);

    let result = provider
        .create_booking(&request(), CallContext::default())
        .await;

    assert!(matches!(result, Err(ProviderError::RateLimited { .. })));
    assert_eq!(adapter.calls(), 1);
}

#[tokio::test]
async fn 짧은_retry_after는_그만큼_기다렸다가_다시_보낸다() {
    let adapter = Scripted::new(NOT_IDEMPOTENT)
        .create(
            MS,
            Err(ProviderError::RateLimited {
                retry_after: Some(100 * MS),
            }),
        )
        .create(MS, booked("rsv_1"));
    let (provider, adapter, mut records) = provider(adapter, fast_policy());

    let started = Instant::now();
    let result = provider
        .create_booking(&request(), CallContext::default())
        .await;

    assert_eq!(result, booked("rsv_1"));
    assert!(started.elapsed() >= 100 * MS);
    assert_eq!(adapter.calls(), 2);
    assert_eq!(
        drain(&mut records),
        vec![(1, "rate_limited"), (2, "success")]
    );
}

#[tokio::test]
async fn 업체가_쉬라고_하면_그동안_호출하지_않는다() {
    let adapter = Scripted::new(IDEMPOTENT)
        .create(
            MS,
            Err(ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(2)),
            }),
        )
        .create(MS, booked("rsv_1"));
    let (provider, adapter, mut records) = provider(adapter, fast_policy());

    // Retry-After가 1초를 넘으므로 같은 호출 안에서는 다시 보내지 않는다
    let first = provider
        .create_booking(&request(), CallContext::default())
        .await;
    assert!(matches!(first, Err(ProviderError::RateLimited { .. })));
    assert!(provider.paused_until().is_some());

    // 쉬는 동안의 호출은 업체에 가지 않는다
    let second = provider
        .create_booking(&request(), CallContext::default())
        .await;
    let Err(ProviderError::Throttled { retry_at }) = second else {
        panic!("pause 중이면 Throttled여야 한다: {second:?}");
    };
    assert!(retry_at > Instant::now() + Duration::from_secs(1));
    assert_eq!(adapter.calls(), 1);
    assert_eq!(
        drain(&mut records),
        vec![(1, "rate_limited"), (1, "throttled")]
    );
}

#[tokio::test]
async fn 서킷이_열리면_어댑터를_부르지_않는다() {
    let adapter = Scripted::new(NOT_IDEMPOTENT)
        .create(MS, Err(ProviderError::Server { status: 503 }))
        .create(MS, Err(ProviderError::Server { status: 503 }))
        .create(MS, booked("rsv_1"));
    let policy = ProviderPolicy {
        circuit_breaker: crate::CircuitBreakerPolicy {
            window: 2,
            min_calls: 2,
            failure_rate: 0.5,
            open_ms: 60_000,
            half_open_probes: 1,
        },
        ..fast_policy()
    };
    let (provider, adapter, mut records) = provider(adapter, policy);

    for _ in 0..2 {
        let result = provider
            .create_booking(&request(), CallContext::default())
            .await;
        assert_eq!(result, Err(ProviderError::Server { status: 503 }));
    }
    assert_eq!(provider.circuit_state(), CircuitState::Open);

    let result = provider
        .create_booking(&request(), CallContext::default())
        .await;
    assert!(matches!(result, Err(ProviderError::CircuitOpen { .. })));
    assert_eq!(adapter.calls(), 2, "열린 뒤에는 부르지 않는다");
    assert_eq!(drain(&mut records).last(), Some(&(1, "circuit_open")));
}

#[tokio::test]
async fn rate_limiter_토큰을_기다릴_수_없으면_호출하지_않는다() {
    let adapter = Scripted::new(IDEMPOTENT)
        .create(MS, booked("rsv_1"))
        .create(MS, booked("rsv_2"));
    let policy = ProviderPolicy {
        rate_limit: RateLimitPolicy {
            per_second: 1,
            burst: 1,
            max_wait_ms: 50,
        },
        ..fast_policy()
    };
    let (provider, adapter, _records) = provider(adapter, policy);

    assert!(
        provider
            .create_booking(&request(), CallContext::default())
            .await
            .is_ok()
    );
    // 다음 토큰은 약 1초 뒤에 생기는데 50ms까지만 기다린다
    let result = provider
        .create_booking(&request(), CallContext::default())
        .await;
    let Err(ProviderError::Throttled { retry_at }) = result else {
        panic!("Throttled여야 한다: {result:?}");
    };
    assert!(
        retry_at > Instant::now() + 500 * MS,
        "토큰이 생기는 시각을 알려 준다"
    );
    assert_eq!(adapter.calls(), 1);
}

#[tokio::test]
async fn rate_limiter는_짧은_대기는_기다렸다가_보낸다() {
    let adapter = Scripted::new(IDEMPOTENT)
        .create(MS, booked("rsv_1"))
        .create(MS, booked("rsv_2"));
    let policy = ProviderPolicy {
        rate_limit: RateLimitPolicy {
            per_second: 10,
            burst: 1,
            max_wait_ms: 500,
        },
        ..fast_policy()
    };
    let (provider, adapter, _records) = provider(adapter, policy);

    let started = Instant::now();
    for _ in 0..2 {
        assert!(
            provider
                .create_booking(&request(), CallContext::default())
                .await
                .is_ok()
        );
    }
    // 초당 10회 → 두 번째 호출은 약 100ms를 기다린다
    assert!(started.elapsed() >= 80 * MS);
    assert_eq!(adapter.calls(), 2);
}

#[tokio::test]
async fn bulkhead가_가득_차면_기다리다가_포기한다() {
    let adapter = Scripted::new(IDEMPOTENT)
        .create(300 * MS, booked("rsv_slow"))
        .create(MS, booked("rsv_2"));
    let policy = ProviderPolicy {
        bulkhead: crate::BulkheadPolicy {
            max_concurrency: 1,
            max_wait_ms: 20,
        },
        ..fast_policy()
    };
    let (provider, adapter, _records) = provider(adapter, policy);

    // tokio::join!: 두 Future를 동시에 진행시키고 둘 다 끝나기를 기다린다 (≈ Task.WhenAll)
    let request = request();
    let (slow, blocked) = tokio::join!(
        provider.create_booking(&request, CallContext::default()),
        async {
            tokio::time::sleep(50 * MS).await;
            provider
                .create_booking(&request, CallContext::default())
                .await
        }
    );

    assert_eq!(slow, booked("rsv_slow"));
    assert!(matches!(blocked, Err(ProviderError::Throttled { .. })));
    assert_eq!(adapter.calls(), 1);
}

#[tokio::test]
async fn 호출_기록에_예약_작업_응답_정보를_남긴다() {
    let adapter = Scripted::new(IDEMPOTENT);
    lock(&adapter.creates).push_back((
        MS,
        AdapterReply {
            result: booked("rsv_1"),
            meta: ResponseMeta {
                http_status: Some(201),
                rate_limit_remaining: Some(4),
            },
        },
    ));
    let (provider, _adapter, mut records) = provider(adapter, fast_policy());
    let booking_id = BookingId::from_uuid(uuid::Uuid::now_v7());

    provider
        .create_booking(
            &request(),
            CallContext {
                booking_id: Some(booking_id),
                job_id: Some(42),
            },
        )
        .await
        .unwrap();

    let record = records.try_recv().unwrap();
    assert_eq!(record.provider_id.as_str(), "alpha");
    assert_eq!(record.operation, "create_booking");
    assert_eq!(record.booking_id, Some(booking_id));
    assert_eq!(record.job_id, Some(42));
    assert_eq!(record.attempt, 1);
    assert_eq!(record.outcome, "success");
    assert_eq!(record.http_status, Some(201));
    assert_eq!(record.rate_limit_remaining, Some(4));
    assert_eq!(record.error_detail, None);
    assert!(record.duration >= MS);
}

#[tokio::test]
async fn 재시도가_서킷에_막혀도_업체까지_간_시도의_실패를_돌려준다() {
    // 첫 시도의 실패로 서킷이 열리고, 같은 호출의 재시도는 서킷에 막힌다
    let adapter = Scripted::new(NOT_IDEMPOTENT).find(Err(ProviderError::Server { status: 503 }));
    let policy = ProviderPolicy {
        circuit_breaker: crate::CircuitBreakerPolicy {
            window: 1,
            min_calls: 1,
            failure_rate: 0.5,
            open_ms: 60_000,
            half_open_probes: 1,
        },
        ..fast_policy()
    };
    let (provider, adapter, mut records) = provider(adapter, policy);

    let result = provider
        .find_booking(request().reference, CallContext::default())
        .await;

    // CircuitOpen을 돌려주면 워커가 "부르지 않았다"로 보고 시도 횟수를 되돌린다. 실제로 불렀으므로 그러면 안 된다.
    assert_eq!(result, Err(ProviderError::Server { status: 503 }));
    assert_eq!(adapter.calls(), 1);
    assert_eq!(
        drain(&mut records),
        vec![(1, "server_error"), (2, "circuit_open")]
    );
}

#[tokio::test]
async fn 시도_타임아웃만큼_예산이_남지_않으면_다시_보내지_않는다() {
    let adapter = Scripted::new(IDEMPOTENT)
        .create(MS, Err(ProviderError::Server { status: 503 }))
        .create(MS, booked("rsv_1"));
    // 예산 500ms 중 첫 시도 뒤 남은 시간 < 시도 타임아웃 1초
    let policy = ProviderPolicy {
        call_budget_ms: 500,
        ..fast_policy()
    };
    let (provider, adapter, _records) = provider(adapter, policy);

    let result = provider
        .create_booking(&request(), CallContext::default())
        .await;

    assert_eq!(result, Err(ProviderError::Server { status: 503 }));
    assert_eq!(adapter.calls(), 1);
}

#[tokio::test]
async fn 관문에서_기다리는_동안_서킷이_열리면_보내지_않는다() {
    // bulkhead 자리가 하나뿐이다. 느린 첫 호출이 자리를 쥔 동안 두 번째 호출이 기다리고,
    // 그사이 첫 호출이 실패해 서킷이 열린다.
    let adapter = Scripted::new(IDEMPOTENT)
        .create(100 * MS, Err(ProviderError::Server { status: 503 }))
        .create(MS, booked("rsv_should_not_happen"));
    let policy = ProviderPolicy {
        retry: crate::RetryPolicy {
            max_attempts: 1,
            ..crate::RetryPolicy::default()
        },
        bulkhead: crate::BulkheadPolicy {
            max_concurrency: 1,
            max_wait_ms: 1_000,
        },
        circuit_breaker: crate::CircuitBreakerPolicy {
            window: 1,
            min_calls: 1,
            failure_rate: 0.5,
            open_ms: 60_000,
            half_open_probes: 1,
        },
        ..fast_policy()
    };
    let (provider, adapter, _records) = provider(adapter, policy);

    let request = request();
    let (first, second) = tokio::join!(
        provider.create_booking(&request, CallContext::default()),
        async {
            tokio::time::sleep(20 * MS).await;
            provider
                .create_booking(&request, CallContext::default())
                .await
        }
    );

    assert_eq!(first, Err(ProviderError::Server { status: 503 }));
    assert!(
        matches!(second, Err(ProviderError::CircuitOpen { .. })),
        "{second:?}"
    );
    assert_eq!(adapter.calls(), 1, "열린 서킷 뒤로 요청이 나가지 않는다");
}

#[test]
fn 최대_호출_시간은_예산과_관문_대기의_합이다() {
    let (provider, _adapter, _records) =
        provider(Scripted::new(IDEMPOTENT), ProviderPolicy::default());
    assert_eq!(
        provider.longest_call(),
        Duration::from_millis(15_000 + 500 + 200)
    );
}
