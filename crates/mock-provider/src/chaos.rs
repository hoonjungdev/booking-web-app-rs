//! 장애 주입 (chaos).
//!
//! 업체 API 요청마다 아래 순서로 장애를 적용한다 (docs/04-api.md §4).
//!
//! 1. `outage`면 바로 503
//! 2. rate limit 초과면 429 + `Retry-After`
//! 3. 지연 (`latency_ms` 범위에서 무작위, 일정 확률로 `slow` 지연)
//! 4. 일정 확률로 5xx (예약을 **만들지 않음**)
//! 5. 일정 확률로 응답하지 않고 대기 (`hang`, 클라이언트 타임아웃 유발)
//! 6. 정상 처리
//! 7. 쓰기 요청이면 일정 확률로 **처리는 하고** 응답하지 않음 (`drop_after_commit`, 결과 불명 상황 재현)

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderName, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::Instrument;

use crate::{
    Provider, lock,
    rate_limit::{Acquire, TokenBucket},
};

/// `hang` · `drop_after_commit`일 때 응답을 붙잡아 두는 시간.
/// 클라이언트 타임아웃(우리 서비스 기본 3초)보다 충분히 길고, mock 서버 종료를 너무 오래 막지 않는 값.
pub const HANG_DURATION: Duration = Duration::from_secs(30);

/// 업체 하나의 장애 설정.
///
/// `#[serde(default)]` 덕분에 PUT 요청에서 일부 필드만 보내면 나머지는 [`Preset::Healthy`] 값으로 채워진다.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChaosConfig {
    pub latency_ms: LatencyRange,
    pub slow: SlowConfig,
    pub error: ErrorConfig,
    pub hang_probability: f64,
    pub drop_after_commit_probability: f64,
    pub rate_limit: RateLimitConfig,
    pub outage: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LatencyRange {
    pub min: u64,
    pub max: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlowConfig {
    pub probability: f64,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorConfig {
    pub probability: f64,
    pub statuses: Vec<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    pub per_second: u32,
    pub burst: u32,
}

impl Default for ChaosConfig {
    fn default() -> Self {
        Preset::Healthy.config()
    }
}

impl ChaosConfig {
    /// 장애도 지연도 없는 설정. 테스트처럼 결과가 항상 같아야 할 때 쓴다.
    pub fn passthrough() -> Self {
        Self {
            latency_ms: LatencyRange { min: 0, max: 0 },
            slow: SlowConfig {
                probability: 0.0,
                latency_ms: 0,
            },
            error: ErrorConfig {
                probability: 0.0,
                statuses: vec![500],
            },
            hang_probability: 0.0,
            drop_after_commit_probability: 0.0,
            rate_limit: RateLimitConfig {
                per_second: 100_000,
                burst: 100_000,
            },
            outage: false,
        }
    }

    /// 값이 말이 되는지 검사한다. 문제가 있으면 이유를 담은 `Err`.
    pub fn validate(&self) -> Result<(), String> {
        let probabilities = [
            ("slow.probability", self.slow.probability),
            ("error.probability", self.error.probability),
            ("hang_probability", self.hang_probability),
            (
                "drop_after_commit_probability",
                self.drop_after_commit_probability,
            ),
        ];
        for (name, p) in probabilities {
            if !(0.0..=1.0).contains(&p) {
                return Err(format!("{name}는 0.0 ~ 1.0 사이여야 합니다 (입력: {p})"));
            }
        }
        if self.latency_ms.min > self.latency_ms.max {
            return Err("latency_ms.min은 latency_ms.max보다 클 수 없습니다".into());
        }
        if self.error.statuses.is_empty() {
            return Err("error.statuses에는 상태 코드가 하나 이상 있어야 합니다".into());
        }
        if let Some(bad) = self
            .error
            .statuses
            .iter()
            .find(|s| !(500..=599).contains(*s))
        {
            return Err(format!(
                "error.statuses에는 5xx만 쓸 수 있습니다 (입력: {bad})"
            ));
        }
        if self.rate_limit.per_second == 0 || self.rate_limit.burst == 0 {
            return Err("rate_limit.per_second와 burst는 1 이상이어야 합니다".into());
        }
        Ok(())
    }
}

/// 미리 정해 둔 장애 시나리오 (docs/03-resilience.md §10)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    /// 정상: 20~120ms 지연, 초당 20회
    Healthy,
    /// 10% 확률로 5xx
    Flaky,
    /// 30% 확률로 3초 지연
    Slow,
    /// 초당 5회 제한
    Throttled,
    /// 쓰기 요청의 20%를 처리한 뒤 응답하지 않음
    Lossy,
    /// 모든 요청에 503
    Outage,
}

impl Preset {
    pub fn config(self) -> ChaosConfig {
        let healthy = ChaosConfig {
            latency_ms: LatencyRange { min: 20, max: 120 },
            slow: SlowConfig {
                probability: 0.0,
                latency_ms: 3000,
            },
            error: ErrorConfig {
                probability: 0.0,
                statuses: vec![500, 502, 503],
            },
            hang_probability: 0.0,
            drop_after_commit_probability: 0.0,
            rate_limit: RateLimitConfig {
                per_second: 20,
                burst: 10,
            },
            outage: false,
        };

        // `..healthy`: 나머지 필드는 healthy에서 가져온다 (≈ C# record의 `with` 식)
        match self {
            Self::Healthy => healthy,
            Self::Flaky => ChaosConfig {
                error: ErrorConfig {
                    probability: 0.1,
                    ..healthy.error.clone()
                },
                ..healthy
            },
            Self::Slow => ChaosConfig {
                slow: SlowConfig {
                    probability: 0.3,
                    ..healthy.slow
                },
                ..healthy
            },
            Self::Throttled => ChaosConfig {
                rate_limit: RateLimitConfig {
                    per_second: 5,
                    burst: 5,
                },
                ..healthy
            },
            Self::Lossy => ChaosConfig {
                drop_after_commit_probability: 0.2,
                ..healthy
            },
            Self::Outage => ChaosConfig {
                outage: true,
                ..healthy
            },
        }
    }
}

/// 업체별 장애 설정과 rate limiter를 보관한다.
pub struct Registry {
    alpha: Mutex<ProviderChaos>,
}

struct ProviderChaos {
    config: ChaosConfig,
    bucket: TokenBucket,
}

impl ProviderChaos {
    fn new(config: ChaosConfig) -> Self {
        let bucket = TokenBucket::new(
            config.rate_limit.per_second,
            config.rate_limit.burst,
            Instant::now(),
        );
        Self { config, bucket }
    }
}

/// rate limit 응답 헤더에 넣을 값
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitHeaders {
    pub limit: u32,
    pub remaining: u32,
    pub reset_secs: u64,
}

/// 요청을 받아도 되는지 판단한 결과
#[derive(Debug, Clone, PartialEq)]
pub enum Admission {
    Outage,
    RateLimited {
        retry_after: Duration,
        headers: RateLimitHeaders,
    },
    Allowed {
        config: ChaosConfig,
        headers: RateLimitHeaders,
    },
}

impl Registry {
    pub fn new(initial: &ChaosConfig) -> Self {
        Self {
            alpha: Mutex::new(ProviderChaos::new(initial.clone())),
        }
    }

    /// 업체별 칸을 고른다. 업체가 늘어나면 match에 줄을 추가해야 컴파일된다.
    fn slot(&self, provider: Provider) -> &Mutex<ProviderChaos> {
        match provider {
            Provider::Alpha => &self.alpha,
        }
    }

    pub fn get(&self, provider: Provider) -> ChaosConfig {
        lock(self.slot(provider)).config.clone()
    }

    pub fn all(&self) -> BTreeMap<Provider, ChaosConfig> {
        Provider::ALL
            .into_iter()
            .map(|p| (p, self.get(p)))
            .collect()
    }

    /// 설정을 바꾼다. rate limiter도 새 설정으로 다시 만든다.
    pub fn set(&self, provider: Provider, config: ChaosConfig) {
        *lock(self.slot(provider)) = ProviderChaos::new(config);
    }

    pub fn reset_all(&self, config: &ChaosConfig) {
        for provider in Provider::ALL {
            self.set(provider, config.clone());
        }
    }

    /// 요청 하나를 받아도 되는지 판단한다. 허용이면 토큰을 하나 쓴다.
    pub fn admit(&self, provider: Provider, now: Instant) -> Admission {
        // 잠금은 이 블록 안에서만 쥔다. await 중에 std Mutex를 쥐고 있으면 안 된다.
        let mut state = lock(self.slot(provider));
        if state.config.outage {
            return Admission::Outage;
        }
        let acquired = state.bucket.try_acquire(now);
        let headers = RateLimitHeaders {
            limit: state.bucket.limit(),
            remaining: match acquired {
                Acquire::Granted { remaining } => remaining,
                Acquire::Denied { .. } => 0,
            },
            reset_secs: state.bucket.time_until_full().as_secs_f64().ceil() as u64,
        };
        match acquired {
            Acquire::Granted { .. } => Admission::Allowed {
                config: state.config.clone(),
                headers,
            },
            Acquire::Denied { retry_after } => Admission::RateLimited {
                retry_after,
                headers,
            },
        }
    }
}

/// 요청 하나에 적용할 장애 계획
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub latency: Duration,
    pub fault: Option<Fault>,
    pub drop_after_commit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// 이 상태 코드로 바로 응답 (처리하지 않음)
    Error(StatusCode),
    /// 처리하지 않고 응답도 하지 않음
    Hang,
}

/// 설정과 난수로 이번 요청의 장애 계획을 정한다.
///
/// 난수 생성기를 인자로 받기 때문에, 테스트에서는 시드를 고정한 생성기를 넣어 결과를 재현할 수 있다.
/// (`impl RngExt` ≈ C#에서 인터페이스 타입을 매개변수로 받는 것)
pub fn plan(config: &ChaosConfig, rng: &mut impl RngExt, is_write: bool) -> Plan {
    let mut latency_ms = rng.random_range(config.latency_ms.min..=config.latency_ms.max);
    if rng.random_bool(config.slow.probability) {
        latency_ms += config.slow.latency_ms;
    }

    let fault = if rng.random_bool(config.error.probability) {
        let index = rng.random_range(0..config.error.statuses.len());
        let status = StatusCode::from_u16(config.error.statuses[index])
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        Some(Fault::Error(status))
    } else if rng.random_bool(config.hang_probability) {
        Some(Fault::Hang)
    } else {
        None
    };

    Plan {
        latency: Duration::from_millis(latency_ms),
        fault,
        drop_after_commit: is_write && rng.random_bool(config.drop_after_commit_probability),
    }
}

/// 미들웨어가 쓰는 상태: 어느 업체의 설정을 적용할지
#[derive(Clone)]
pub struct ChaosContext {
    registry: Arc<Registry>,
    provider: Provider,
}

impl ChaosContext {
    pub fn new(registry: Arc<Registry>, provider: Provider) -> Self {
        Self { registry, provider }
    }
}

/// 업체 API 앞에 붙는 axum 미들웨어 (≈ ASP.NET Core의 `app.Use(async (ctx, next) => ...)`)
pub async fn middleware(State(ctx): State<ChaosContext>, request: Request, next: Next) -> Response {
    let is_write = matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );

    let (config, headers) = match ctx.registry.admit(ctx.provider, Instant::now()) {
        Admission::Outage => {
            tracing::debug!(provider = ?ctx.provider, "chaos: outage");
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "업체 서비스 점검 중",
            );
        }
        Admission::RateLimited {
            retry_after,
            headers,
        } => {
            tracing::debug!(provider = ?ctx.provider, ?retry_after, "chaos: rate limited");
            let mut response = error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "호출 한도를 초과했습니다",
            );
            // Retry-After는 초 단위 정수. 1초 미만이어도 최소 1로 올린다.
            let seconds = retry_after.as_secs_f64().ceil().max(1.0) as u64;
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
            add_rate_limit_headers(&mut response, headers);
            return response;
        }
        Admission::Allowed { config, headers } => (config, headers),
    };

    // ThreadRng는 스레드를 넘겨 보낼 수 없는 타입이라 await 전에 이 블록 안에서 쓰고 버린다.
    let plan = {
        let mut rng = rand::rng();
        plan(&config, &mut rng, is_write)
    };
    if plan.fault.is_some() || plan.drop_after_commit {
        tracing::debug!(provider = ?ctx.provider, ?plan, "chaos: injecting fault");
    }

    if !plan.latency.is_zero() {
        // 주입한 지연을 span으로 남긴다. 호출한 쪽의 trace에서 "업체 안에서 이만큼 기다렸다"가 한 줄로 보인다.
        let latency_ms = plan.latency.as_millis() as u64;
        tokio::time::sleep(plan.latency)
            .instrument(tracing::info_span!("chaos.delay", latency_ms))
            .await;
    }

    let mut response = match plan.fault {
        Some(Fault::Error(status)) => error_response(status, "internal_error", "업체 내부 오류"),
        Some(Fault::Hang) => hang().await,
        None => {
            let response = next.run(request).await;
            if plan.drop_after_commit {
                // 처리는 끝났지만(예약 생성됨) 응답을 보내지 않는다.
                hang().await
            } else {
                response
            }
        }
    };
    add_rate_limit_headers(&mut response, headers);
    response
}

/// 응답을 오래 붙잡아 클라이언트 타임아웃을 일으킨다.
/// 클라이언트가 먼저 연결을 끊으면 axum이 이 Future를 버리므로 대기도 함께 끝난다.
async fn hang() -> Response {
    tokio::time::sleep(HANG_DURATION)
        .instrument(tracing::info_span!("chaos.hang"))
        .await;
    error_response(StatusCode::GATEWAY_TIMEOUT, "timeout", "응답 시간 초과")
}

/// alpha 형식의 에러 응답. beta를 추가할 때(Phase 6) 업체별 형식으로 나눈다.
fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({ "error": { "code": code, "message": message } });
    (status, Json(body)).into_response()
}

fn add_rate_limit_headers(response: &mut Response, headers: RateLimitHeaders) {
    let map = response.headers_mut();
    map.insert(
        HeaderName::from_static("x-ratelimit-limit"),
        HeaderValue::from(headers.limit),
    );
    map.insert(
        HeaderName::from_static("x-ratelimit-remaining"),
        HeaderValue::from(headers.remaining),
    );
    map.insert(
        HeaderName::from_static("x-ratelimit-reset"),
        HeaderValue::from(headers.reset_secs),
    );
}

#[cfg(test)]
mod tests {
    use rand::{SeedableRng, rngs::StdRng};

    use super::*;

    #[test]
    fn 모든_프리셋은_검증을_통과한다() {
        for preset in [
            Preset::Healthy,
            Preset::Flaky,
            Preset::Slow,
            Preset::Throttled,
            Preset::Lossy,
            Preset::Outage,
        ] {
            assert_eq!(preset.config().validate(), Ok(()), "{preset:?}");
        }
        assert_eq!(ChaosConfig::passthrough().validate(), Ok(()));
    }

    #[test]
    fn 잘못된_설정은_거절한다() {
        let mut config = ChaosConfig::passthrough();
        config.hang_probability = 1.5;
        assert!(config.validate().is_err());

        let mut config = ChaosConfig::passthrough();
        config.error.statuses = vec![404];
        assert!(config.validate().is_err());
    }

    #[test]
    fn 오류_확률이_1이면_항상_오류다() {
        let mut config = ChaosConfig::passthrough();
        config.error.probability = 1.0;
        config.error.statuses = vec![502];
        let mut rng = StdRng::seed_from_u64(7);

        for _ in 0..100 {
            let plan = plan(&config, &mut rng, false);
            assert_eq!(plan.fault, Some(Fault::Error(StatusCode::BAD_GATEWAY)));
        }
    }

    #[test]
    fn drop_after_commit은_쓰기_요청에만_적용된다() {
        let mut config = ChaosConfig::passthrough();
        config.drop_after_commit_probability = 1.0;
        let mut rng = StdRng::seed_from_u64(7);

        assert!(plan(&config, &mut rng, true).drop_after_commit);
        assert!(!plan(&config, &mut rng, false).drop_after_commit);
    }

    #[test]
    fn 지연은_설정한_범위_안에서_정해진다() {
        let mut config = ChaosConfig::passthrough();
        config.latency_ms = LatencyRange { min: 10, max: 20 };
        let mut rng = StdRng::seed_from_u64(7);

        for _ in 0..100 {
            let latency = plan(&config, &mut rng, false).latency;
            assert!((10..=20).contains(&latency.as_millis()));
        }
    }

    #[test]
    fn outage면_토큰을_쓰지_않고_거절한다() {
        let registry = Registry::new(&Preset::Outage.config());
        assert_eq!(
            registry.admit(Provider::Alpha, Instant::now()),
            Admission::Outage
        );
    }
}
