//! 예약 · 작업 큐 · 업체 상태 지표 (docs/05-observability-and-analytics.md §4)
//!
//! | 이름 | 종류 | 라벨 | 기록하는 곳 |
//! |---|---|---|---|
//! | `booking.transitions` | counter | provider, from, to | 상태 전이를 커밋한 직후 |
//! | `booking.time_to_final` | histogram (초) | provider, final_status | 제출 흐름이 끝나는 상태가 됐을 때 |
//! | `booking.unknown.oldest_age` | gauge (초) | | 큐 상태 태스크 (DB 조회) |
//! | `jobs.lag` | histogram (초) | kind | 워커가 작업을 가져갔을 때 |
//! | `jobs.ready`, `jobs.dead` | gauge | kind | 큐 상태 태스크 (DB 조회) |
//! | `reconciliation.issues.open` | gauge | kind | 큐 상태 태스크 (DB 조회). 이중 예약 의심 알림의 원천 |
//! | `provider.circuit.state` 등 5개 | gauge | provider | 내보낼 때마다 업체 파이프라인의 현재 상태를 읽음 |
//!
//! 업체 호출 시도마다 쌓이는 지표(`provider.call.duration` 등)는 providers 크레이트가, HTTP 요청 지표는 telemetry 크레이트가 기록한다.
//!
//! **라벨에는 값의 종류가 정해진 것만 쓴다.** `booking_id`처럼 끝없이 늘어나는 값을 라벨로 쓰면 시계열이 그만큼 생겨
//! 메트릭 저장소가 감당하지 못한다. 그런 값은 추적 속성과 로그에 남긴다.

use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{Arc, LazyLock, Weak},
    time::Duration,
};

use chrono::Utc;
use db::{
    DbError, PgPool,
    jobs::{Job, JobKind},
    reconciliation::IssueKind,
};
use domain::{Booking, BookingStatus};
use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Gauge, Histogram},
};
use providers::CircuitState;
use tokio_util::sync::CancellationToken;

use crate::registry::ProviderRegistry;

/// 큐 상태(`jobs.ready` 등)를 DB에서 다시 읽는 간격
const QUEUE_GAUGE_INTERVAL: Duration = Duration::from_secs(5);

/// 길게 걸릴 수 있는 시간을 재는 히스토그램의 버킷 경계 (초).
///
/// 업체 호출이나 HTTP 요청은 길어야 몇 초라서 10초까지의 버킷([`telemetry::SECONDS_BUCKETS`])이면 충분하다.
/// 하지만 작업 지연과 "접수 → 종료" 시간은 재시도 · 장애가 끼면 몇 분이 된다. 버킷이 10초에서 끝나면
/// 그 위는 모두 "10초 초과" 한 칸에 들어가서 "작업 지연 p95가 30초 초과" 같은 알림을 계산할 수 없다.
const LONG_SECONDS_BUCKETS: [f64; 17] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    1800.0,
];

struct Instruments {
    transitions: Counter<u64>,
    time_to_final: Histogram<f64>,
    unknown_oldest_age: Gauge<f64>,
    jobs_lag: Histogram<f64>,
    jobs_ready: Gauge<u64>,
    jobs_dead: Gauge<u64>,
    issues_open: Gauge<u64>,
}

/// 처음 쓸 때 한 번 만들어지는 계측기 묶음 (≈ C#의 `static readonly Counter<long> ...`).
/// 그 시점의 전역 meter provider에 묶이므로 `telemetry::init`을 먼저 해야 한다.
static INSTRUMENTS: LazyLock<Instruments> = LazyLock::new(|| {
    let meter = global::meter("booking-server");
    Instruments {
        transitions: meter
            .u64_counter("booking.transitions")
            .with_description("예약 상태 전이 횟수")
            .build(),
        time_to_final: meter
            .f64_histogram("booking.time_to_final")
            .with_unit("s")
            .with_description(
                "접수부터 제출 흐름이 끝나는 상태(confirmed · rejected · failed)까지 걸린 시간",
            )
            .with_boundaries(LONG_SECONDS_BUCKETS.to_vec())
            .build(),
        unknown_oldest_age: meter
            .f64_gauge("booking.unknown.oldest_age")
            .with_unit("s")
            .with_description("결과 불명 상태로 가장 오래 머문 예약의 경과 시간 (없으면 0)")
            .build(),
        jobs_lag: meter
            .f64_histogram("jobs.lag")
            .with_unit("s")
            .with_description("작업을 실행하기로 한 시각부터 워커가 실제로 가져가기까지 밀린 시간")
            .with_boundaries(LONG_SECONDS_BUCKETS.to_vec())
            .build(),
        jobs_ready: meter
            .u64_gauge("jobs.ready")
            .with_description("지금 실행할 수 있는데 아직 가져가지 않은 작업 수")
            .build(),
        jobs_dead: meter
            .u64_gauge("jobs.dead")
            .with_description("더 시도하지 않고 묻어 둔 작업 수 (관리자 확인 대상)")
            .build(),
        issues_open: meter
            .u64_gauge("reconciliation.issues.open")
            .with_description(
                "대사에서 찾은 미해결 불일치 수 (duplicate_suspected는 이중 예약 의심)",
            )
            .build(),
    }
});

/// 예약을 새로 접수했다. 상태 전이 흐름의 시작점으로 센다 (`from="none"`).
pub fn booking_accepted(booking: &Booking) {
    INSTRUMENTS.transitions.add(
        1,
        &[
            provider_label(booking),
            KeyValue::new("from", "none"),
            KeyValue::new("to", booking.status.as_str()),
        ],
    );
}

/// 예약 상태가 `booking.status`에서 `to`로 바뀌었다. **DB에 커밋한 뒤에** 부른다
/// (커밋 전에 세면, 커밋이 실패했을 때 일어나지 않은 전이가 지표에 남는다).
pub fn booking_transitioned(booking: &Booking, to: BookingStatus) {
    INSTRUMENTS.transitions.add(
        1,
        &[
            provider_label(booking),
            KeyValue::new("from", booking.status.as_str()),
            KeyValue::new("to", to.as_str()),
        ],
    );
    if ends_submission(to) {
        // 접수 시각은 DB 시계, 지금은 이 서버 시계다. 시계가 어긋나 음수가 나오면 0으로 본다.
        let elapsed = (Utc::now() - booking.created_at)
            .to_std()
            .unwrap_or(Duration::ZERO);
        INSTRUMENTS.time_to_final.record(
            elapsed.as_secs_f64(),
            &[
                provider_label(booking),
                KeyValue::new("final_status", to.as_str()),
            ],
        );
    }
}

/// 접수한 예약의 제출 흐름이 끝나는 상태인가: 확정됐거나, 업체가 거절했거나, 포기했거나.
///
/// 도메인의 "종료 상태"(`BookingStatus::is_terminal`: 더는 바뀌지 않는 상태)와는 다르다. `confirmed`는 나중에 취소될 수 있고
/// `failed`는 관리자가 다시 시도할 수 있지만, "접수한 예약이 결론을 얻기까지 얼마나 걸렸나"를 재는 기준으로는 여기가 끝이다.
fn ends_submission(status: BookingStatus) -> bool {
    matches!(
        status,
        BookingStatus::Confirmed | BookingStatus::Rejected | BookingStatus::Failed
    )
}

/// 워커가 작업을 가져갔다
pub fn job_claimed(job: &Job) {
    INSTRUMENTS
        .jobs_lag
        .record(job.lag.as_secs_f64(), &[kind_label(&job.kind)]);
}

/// 종료 신호가 올 때까지 큐 상태 지표를 주기적으로 갱신한다.
///
/// 이 값들은 DB에 물어야 알 수 있다. 지표를 내보내는 SDK 콜백은 동기 함수라 DB를 기다릴 수 없으므로,
/// 태스크가 따로 읽어 **마지막 값을 기억하는 계측기(gauge)**에 넣어 둔다.
pub async fn run_queue_gauges(pool: PgPool, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(QUEUE_GAUGE_INTERVAL);
    // 조회가 늦어져 주기를 놓쳤을 때, 놓친 만큼 몰아서 조회하지 않고 그때부터 다시 간격을 둔다
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }
        if let Err(error) = refresh_queue_gauges(&pool).await {
            // DB가 잠깐 끊긴 경우 등. 지표는 마지막 값에 머물고, 다음 주기에 다시 시도한다.
            tracing::warn!(%error, "큐 상태 지표를 갱신하지 못했다");
        }
    }
}

/// 큐 상태 지표를 한 번 갱신한다 (`jobs.ready`, `jobs.dead`, `booking.unknown.oldest_age`, `reconciliation.issues.open`)
pub async fn refresh_queue_gauges(pool: &PgPool) -> Result<(), DbError> {
    let depths = db::jobs::queue_depths(pool).await?;

    // 종류별 (대기, dead) 수. 아는 종류와 `unknown`을 0으로 채워 두고 시작한다: 해당하는 작업이 없는 종류는
    // 조회 결과에 없는데, 그대로 두면 지표가 마지막 값(예: 3)에 머무른다.
    let mut totals: BTreeMap<&'static str, (u64, u64)> = JobKind::ALL
        .into_iter()
        .map(JobKind::as_str)
        .chain([UNKNOWN_KIND])
        .map(|kind| (kind, (0, 0)))
        .collect();
    for depth in &depths {
        // 이 코드가 모르는 종류는 `unknown`으로 묶는다. 워커는 그런 작업을 dead로 묻으므로,
        // 여기서 빼 버리면 "dead 작업이 있음" 알림이 울리지 않는다.
        let (ready, dead) = totals.entry(kind_name(&depth.kind)).or_default();
        *ready += count(depth.ready);
        *dead += count(depth.dead);
    }
    for (kind, (ready, dead)) in totals {
        let label = [KeyValue::new("kind", kind)];
        INSTRUMENTS.jobs_ready.record(ready, &label);
        INSTRUMENTS.jobs_dead.record(dead, &label);
    }

    let oldest_unknown = db::bookings::oldest_unknown_age(pool).await?;
    INSTRUMENTS
        .unknown_oldest_age
        .record(oldest_unknown.as_secs_f64(), &[]);

    // 미해결 대사 이슈 수. 이슈가 없는 종류도 0으로 내보낸다 (처리 완료로 바뀌면 0으로 내려가야 알림이 풀린다).
    let mut open: BTreeMap<IssueKind, u64> =
        IssueKind::ALL.into_iter().map(|kind| (kind, 0)).collect();
    for (kind, issues) in db::reconciliation::open_counts(pool).await? {
        open.insert(kind, count(issues));
    }
    for (kind, issues) in open {
        INSTRUMENTS
            .issues_open
            .record(issues, &[KeyValue::new("kind", kind.as_str())]);
    }
    Ok(())
}

/// 업체 파이프라인의 현재 상태를 지표로 내보낸다 (서킷, pause, 동시 실행 수, 업체가 알려 준 남은 호출 수).
///
/// **관측형(observable) gauge**: 값을 그때그때 기록하지 않고, 지표를 내보낼 때마다 SDK가 콜백을 불러 현재 값을 읽어 간다
/// (≈ .NET의 `Meter.CreateObservableGauge`). pause처럼 "시간이 지나면 저절로 풀리는" 상태는 바뀌는 순간을 잡아
/// 기록할 수 없어서, 내보내는 순간에 읽는 쪽이 정확하다.
///
/// 콜백은 레지스트리를 `Weak`(약한 참조)로 쥔다. `Arc`로 쥐면 콜백이 살아 있는 한 업체 파이프라인도 사라지지 않고,
/// 그 안의 호출 기록 채널이 닫히지 않아 서버가 종료되지 못한다 (docs/decisions.md D-18).
/// `Weak`는 값이 아직 살아 있으면 `upgrade()`로 잠깐 빌려 쓸 수 있지만, 그것만으로 값을 살려 두지는 않는다
/// (≈ C#의 `WeakReference<T>`).
pub fn register_provider_gauges(registry: &Arc<ProviderRegistry>) {
    let meter = global::meter("booking-server");

    let providers = Arc::downgrade(registry);
    let _circuit = meter
        .u64_observable_gauge("provider.circuit.state")
        .with_description("서킷 브레이커 상태 (0 = closed, 1 = half_open, 2 = open)")
        .with_callback(move |observer| {
            for_each_provider(&providers, |provider, label| {
                observer.observe(circuit_code(provider.circuit_state()), label);
            });
        })
        .build();

    let providers = Arc::downgrade(registry);
    let _paused = meter
        .u64_observable_gauge("provider.paused")
        .with_description("업체가 쉬라고 해서(Retry-After) 호출을 멈춘 상태면 1")
        .with_callback(move |observer| {
            for_each_provider(&providers, |provider, label| {
                observer.observe(u64::from(provider.paused_until().is_some()), label);
            });
        })
        .build();

    let providers = Arc::downgrade(registry);
    let _in_flight = meter
        .u64_observable_gauge("provider.bulkhead.in_flight")
        .with_description("지금 업체를 부르고 있는 호출 수")
        .with_callback(move |observer| {
            for_each_provider(&providers, |provider, label| {
                observer.observe(provider.in_flight() as u64, label);
            });
        })
        .build();

    let providers = Arc::downgrade(registry);
    let _allowed = meter
        .u64_observable_gauge("provider.rate_limit.allowed")
        .with_description(
            "우리 rate limiter가 지금 허용하는 초당 호출 수 (업체가 한도 초과를 알리면 AIMD로 줄었다가 되돌아간다)",
        )
        .with_callback(move |observer| {
            for_each_provider(&providers, |provider, label| {
                observer.observe(u64::from(provider.rate_limit_allowed()), label);
            });
        })
        .build();

    let providers = Arc::downgrade(registry);
    let _remaining = meter
        .u64_observable_gauge("provider.rate_limit.remaining")
        .with_description("업체가 마지막 응답에서 알려 준 남은 호출 수 (X-RateLimit-Remaining)")
        .with_callback(move |observer| {
            for_each_provider(&providers, |provider, label| {
                // 아직 응답을 받은 적이 없으면 내보낼 값이 없다
                if let Some(remaining) = provider.rate_limit_remaining() {
                    observer.observe(u64::from(remaining), label);
                }
            });
        })
        .build();
}

/// 레지스트리가 아직 살아 있으면 업체마다 `observe`를 부른다. 두 번째 인자는 그 업체의 라벨이다.
fn for_each_provider(
    registry: &Weak<ProviderRegistry>,
    mut observe: impl FnMut(&providers::ResilientProvider, &[KeyValue]),
) {
    let Some(registry) = registry.upgrade() else {
        return; // 서버가 종료 중이라 업체 파이프라인이 이미 정리됐다
    };
    for provider in registry.all() {
        let label = [KeyValue::new("provider", provider.id().to_string())];
        observe(provider, &label);
    }
}

fn circuit_code(state: CircuitState) -> u64 {
    match state {
        CircuitState::Closed => 0,
        CircuitState::HalfOpen => 1,
        CircuitState::Open => 2,
    }
}

fn provider_label(booking: &Booking) -> KeyValue {
    KeyValue::new("provider", booking.request.provider.to_string())
}

/// 이 코드가 모르는 작업 종류에 붙이는 라벨 값
const UNKNOWN_KIND: &str = "unknown";

/// 지표 라벨로 쓸 작업 종류. DB에는 이 코드가 모르는 종류가 들어 있을 수도 있어서(다른 버전이 등록한 작업 등),
/// 아는 종류가 아니면 `unknown`으로 묶는다. 값을 그대로 쓰면 라벨 값의 종류를 우리가 정할 수 없게 된다.
fn kind_name(kind: &str) -> &'static str {
    JobKind::from_str(kind).map_or(UNKNOWN_KIND, JobKind::as_str)
}

fn kind_label(kind: &str) -> KeyValue {
    KeyValue::new("kind", kind_name(kind))
}

/// DB의 `count(*)`는 부호 있는 정수(`bigint`)로 온다. 음수일 수는 없지만 타입을 맞추려고 바꾼다.
fn count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 제출_흐름이_끝나는_상태는_확정_거절_실패다() {
        let ended: Vec<_> = BookingStatus::ALL
            .into_iter()
            .filter(|status| ends_submission(*status))
            .collect();
        assert_eq!(
            ended,
            vec![
                BookingStatus::Confirmed,
                BookingStatus::Rejected,
                BookingStatus::Failed
            ]
        );
    }

    #[test]
    fn 서킷_상태_코드는_문서와_같다() {
        // docs/05-observability-and-analytics.md §4: 0=closed, 1=half_open, 2=open
        assert_eq!(circuit_code(CircuitState::Closed), 0);
        assert_eq!(circuit_code(CircuitState::HalfOpen), 1);
        assert_eq!(circuit_code(CircuitState::Open), 2);
    }

    #[test]
    fn 모르는_작업_종류는_하나로_묶는다() {
        assert_eq!(
            kind_label("submit_booking").value.as_str(),
            "submit_booking"
        );
        assert_eq!(kind_label("something_new").value.as_str(), "unknown");
    }
}
