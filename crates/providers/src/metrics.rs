//! 업체 호출 지표 (docs/05-observability-and-analytics.md §4)
//!
//! | 이름 | 종류 | 라벨 |
//! |---|---|---|
//! | `provider.call.duration` | histogram (초) | provider, operation, outcome |
//! | `provider.call.retries` | counter | provider, operation |
//! | `provider.throttled` | counter | provider, source (`local` \| `remote`) |
//! | `provider_calls.recorder.dropped` | counter | |
//!
//! 이 크레이트는 OpenTelemetry **API**만 쓴다. 계측기를 만들어 값을 기록할 뿐, 어디로 어떻게 내보낼지는 모른다
//! (≈ .NET 라이브러리가 `System.Diagnostics.Metrics.Meter`만 쓰고, OpenTelemetry SDK 구성은 앱이 하는 것).
//! 실행 파일이 SDK를 설치하지 않았으면(테스트 등) 기록은 아무 일도 하지 않는다. → docs/decisions.md D-20
//!
//! 서킷 상태 · pause · 동시 실행 수 같은 **현재 상태**는 여기서 기록하지 않는다. `ResilientProvider`가 값을 내주고
//! (`circuit_state()`, `paused_until()` …), booking-server가 지표를 내보낼 때마다 읽어 간다.

use std::{sync::LazyLock, time::Duration};

use domain::ProviderId;
use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Histogram},
};

use crate::resilience::Operation;

/// 초 단위 히스토그램의 버킷 경계 (docs/05-observability-and-analytics.md §4, `telemetry::SECONDS_BUCKETS`와 같은 값).
/// OpenTelemetry의 기본 버킷은 밀리초 값에 맞춰져 있어서 직접 지정한다.
const SECONDS_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

struct Instruments {
    call_duration: Histogram<f64>,
    retries: Counter<u64>,
    throttled: Counter<u64>,
    recorder_dropped: Counter<u64>,
}

/// 계측기는 프로세스에 한 벌만 있으면 된다. `LazyLock`: 처음 쓸 때 한 번 만들어지는 전역 값
/// (≈ C#의 `static readonly` 필드 + `Lazy<T>`).
///
/// 처음 쓰는 시점의 전역 meter provider에 묶인다. 그래서 실행 파일은 업체를 부르기 전에 `telemetry::init`을 먼저 해야 한다.
static INSTRUMENTS: LazyLock<Instruments> = LazyLock::new(|| {
    let meter = global::meter("providers");
    Instruments {
        call_duration: meter
            .f64_histogram("provider.call.duration")
            .with_unit("s")
            .with_description("업체 호출 시도 1회에 걸린 시간")
            .with_boundaries(SECONDS_BUCKETS.to_vec())
            .build(),
        retries: meter
            .u64_counter("provider.call.retries")
            .with_description("같은 호출 안에서 다시 시도한 횟수")
            .build(),
        throttled: meter
            .u64_counter("provider.throttled")
            .with_description("호출 제한에 걸린 횟수 (local: 우리 쪽에서 막음, remote: 업체가 429 · 503으로 알림)")
            .build(),
        recorder_dropped: meter
            .u64_counter("provider_calls.recorder.dropped")
            .with_description("호출 기록 채널이 가득 차서 버린 건수")
            .build(),
    }
});

/// 호출 제한이 어디서 걸렸는가 (`provider.throttled`의 `source` 라벨)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThrottleSource {
    /// 우리 쪽에서 막아 업체를 부르지 않았다 (rate limiter · bulkhead 대기 초과, 업체 pause 중)
    Local,
    /// 업체가 한도 초과를 알렸다 (429, 503 + Retry-After)
    Remote,
}

impl ThrottleSource {
    const ALL: [Self; 2] = [Self::Local, Self::Remote];

    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

/// 이 업체의 counter 시계열을 0으로 만들어 둔다 (업체 파이프라인을 만들 때 부른다).
///
/// counter는 한 번이라도 올라가야 시계열이 생긴다. 그런데 Prometheus는 **처음 나타난 값을 증가로 보지 않는다**
/// (그 전 값을 모르므로). 그대로 두면 서버를 띄운 뒤 첫 재시도 · 첫 호출 제한이 그래프와 알림에서 빠진다.
/// 0을 한 번 더해 "0에서 시작했다"는 것을 알려 두면 첫 증가부터 잡힌다.
pub(crate) fn register_provider(provider: &ProviderId) {
    for operation in Operation::ALL {
        INSTRUMENTS.retries.add(
            0,
            &[
                provider_label(provider),
                KeyValue::new("operation", operation.as_str()),
            ],
        );
    }
    for source in ThrottleSource::ALL {
        INSTRUMENTS.throttled.add(
            0,
            &[
                provider_label(provider),
                KeyValue::new("source", source.as_str()),
            ],
        );
    }
}

/// 호출 기록 채널을 만들 때 부른다. 버린 건수 counter를 0에서 시작하게 한다 (이유는 [`register_provider`]와 같다).
pub(crate) fn register_recorder() {
    INSTRUMENTS.recorder_dropped.add(0, &[]);
}

/// 시도 하나가 끝났다. 관문에서 막혀 부르지 않은 시도(`circuit_open`, `throttled`)도 결과별 비율을 보기 위해 기록한다.
/// 그런 시도는 0초에 가까우므로, 지연 분위수(p95 등)를 볼 때는 `outcome` 라벨로 빼야 한다.
pub(crate) fn attempt_finished(
    provider: &ProviderId,
    operation: &'static str,
    outcome: &'static str,
    duration: Duration,
) {
    INSTRUMENTS.call_duration.record(
        duration.as_secs_f64(),
        &[
            provider_label(provider),
            KeyValue::new("operation", operation),
            KeyValue::new("outcome", outcome),
        ],
    );
}

pub(crate) fn retried(provider: &ProviderId, operation: &'static str) {
    INSTRUMENTS.retries.add(
        1,
        &[
            provider_label(provider),
            KeyValue::new("operation", operation),
        ],
    );
}

pub(crate) fn throttled(provider: &ProviderId, source: ThrottleSource) {
    INSTRUMENTS.throttled.add(
        1,
        &[
            provider_label(provider),
            KeyValue::new("source", source.as_str()),
        ],
    );
}

pub(crate) fn record_dropped() {
    INSTRUMENTS.recorder_dropped.add(1, &[]);
}

/// 라벨에는 값의 종류가 정해져 있는 것만 쓴다. `booking_id`처럼 끝없이 늘어나는 값은 넣지 않는다 (추적 속성에 넣는다).
fn provider_label(provider: &ProviderId) -> KeyValue {
    KeyValue::new("provider", provider.to_string())
}
