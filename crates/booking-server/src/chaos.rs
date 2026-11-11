//! mock 업체의 장애 주입 API(`/_chaos`) 중계 (개발 환경 전용, docs/04-api.md §3 · §4)
//!
//! 관리자 웹은 booking-server 주소 하나만 안다(`/admin/api/...`). 장애 주입은 mock 업체가 따로 띄운 API라서,
//! 관리자 API가 대신 불러 준다. 주소는 `BOOKING_CHAOS_URL`로 켠다 (설정하지 않으면 이 기능이 꺼진다).
//!
//! 장애 설정의 모양은 mock-provider의 `ChaosConfig`와 같다. mock-provider는 진짜 외부 업체처럼 우리 코드와 독립이어야
//! 하므로(의존하지 않음) 여기에 같은 모양의 타입을 따로 둔다. mock 쪽이 바뀌면 이 파일도 함께 고친다.
//!
//! 설정 값은 보내기 전에 여기서 검사한다 ([`ChaosSettings::validate`]). 상대가 보낸 글자(오류 본문)는 오류 메시지에
//! 넣지 않는다는 규칙(CLAUDE.md)을 지키면서도, 관리자에게 어느 값이 틀렸는지 알려 주기 위해서다.

use std::{collections::BTreeMap, time::Duration};

use domain::ProviderId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 업체 하나의 장애 설정 (mock-provider `ChaosConfig`와 같은 모양)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ChaosSettings {
    /// 모든 요청에 이 범위의 무작위 지연 (밀리초)
    pub latency_ms: LatencyRange,
    /// 일정 확률로 지연을 더한다
    pub slow: SlowSettings,
    /// 일정 확률로 5xx (예약을 만들지 않음)
    pub error: ErrorSettings,
    /// 처리하지 않고 30초 동안 응답하지 않을 확률 (0~1)
    pub hang_probability: f64,
    /// 쓰기 요청만: 처리는 하고 30초 동안 응답하지 않을 확률 (0~1). 결과 불명 상황을 만든다
    pub drop_after_commit_probability: f64,
    /// 업체 쪽 호출 한도 (토큰 버킷)
    pub rate_limit: RateLimitSettings,
    /// 모든 요청에 503
    pub outage: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LatencyRange {
    pub min: u64,
    pub max: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SlowSettings {
    /// 0~1
    pub probability: f64,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ErrorSettings {
    /// 0~1
    pub probability: f64,
    /// 무작위로 고를 상태 코드 (5xx만)
    pub statuses: Vec<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RateLimitSettings {
    /// 초당 다시 채워지는 토큰 수
    pub per_second: u32,
    /// 최대로 쌓이는 토큰 수
    pub burst: u32,
}

impl ChaosSettings {
    /// 값이 말이 되는지 검사한다 (mock-provider의 검사와 같은 규칙). 문제가 있으면 이유를 담은 `Err`.
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
            .find(|status| !(500..=599).contains(*status))
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChaosPreset {
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

impl ChaosPreset {
    fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Flaky => "flaky",
            Self::Slow => "slow",
            Self::Throttled => "throttled",
            Self::Lossy => "lossy",
            Self::Outage => "outage",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChaosError {
    #[error("mock 업체에 {0} 업체가 없습니다")]
    UnknownProvider(ProviderId),
    /// mock 업체가 설정 값을 거절했다 (우리 검사를 통과했는데도). mock이 보낸 본문은 메시지에 넣지 않는다.
    #[error("mock 업체가 장애 설정을 거절했습니다 (HTTP 400). 설정 값을 확인하세요")]
    Rejected,
    #[error("mock 업체의 장애 주입 API를 부를 수 없습니다: {0}")]
    Unreachable(String),
    #[error("mock 업체의 장애 주입 API가 HTTP {0}으로 응답했습니다")]
    Status(u16),
}

/// mock 업체 장애 주입 API 클라이언트. `reqwest::Client`는 내부에서 연결을 공유하므로 복제해도 가볍다.
#[derive(Debug, Clone)]
pub struct ChaosClient {
    http: reqwest::Client,
    /// 예: `http://127.0.0.1:9090`
    base_url: String,
}

impl ChaosClient {
    pub fn new(base_url: &str) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    /// 업체의 지금 장애 설정
    pub async fn get(&self, provider: &ProviderId) -> Result<ChaosSettings, ChaosError> {
        // mock은 모든 업체의 설정을 한 번에 돌려준다 (`GET /_chaos`)
        let all: BTreeMap<String, ChaosSettings> = self
            .send(self.http.get(format!("{}/_chaos", self.base_url)))
            .await?;
        all.get(provider.as_str())
            .cloned()
            .ok_or_else(|| ChaosError::UnknownProvider(provider.clone()))
    }

    /// 장애 설정을 바꾼다. 바뀐 설정을 돌려준다.
    pub async fn put(
        &self,
        provider: &ProviderId,
        settings: &ChaosSettings,
    ) -> Result<ChaosSettings, ChaosError> {
        // mock은 모르는 업체를 "요청 경로 오류"(400)로 거절해 설정 값 오류와 구분되지 않는다. 먼저 있는지 확인한다.
        self.get(provider).await?;
        let url = format!("{}/_chaos/{provider}", self.base_url);
        self.send(self.http.put(url).json(settings)).await
    }

    /// 미리 정해 둔 시나리오를 적용한다
    pub async fn apply_preset(
        &self,
        provider: &ProviderId,
        preset: ChaosPreset,
    ) -> Result<ChaosSettings, ChaosError> {
        self.get(provider).await?;
        let url = format!(
            "{}/_chaos/{provider}/preset/{}",
            self.base_url,
            preset.as_str()
        );
        self.send(self.http.post(url)).await
    }

    /// 요청을 보내고 JSON 응답을 읽는다. 지금 요청의 trace 정보(`traceparent`)를 실어 mock 쪽 처리도 같은 trace에 남긴다.
    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, ChaosError> {
        let request = telemetry::current_carrier()
            .into_iter()
            .fold(request, |request, (name, value)| {
                request.header(name, value)
            });
        let response = request
            .send()
            .await
            .map_err(|error| ChaosError::Unreachable(error.to_string()))?;
        let status = response.status();
        if status == reqwest::StatusCode::BAD_REQUEST {
            // 상대가 보낸 글자는 오류 메시지 · 로그에 넣지 않는다 (CLAUDE.md). 값은 보내기 전에 우리가 검사한다.
            return Err(ChaosError::Rejected);
        }
        if !status.is_success() {
            return Err(ChaosError::Status(status.as_u16()));
        }
        response
            .json()
            .await
            .map_err(|error| ChaosError::Unreachable(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> ChaosSettings {
        ChaosSettings {
            latency_ms: LatencyRange { min: 20, max: 120 },
            slow: SlowSettings {
                probability: 0.0,
                latency_ms: 3000,
            },
            error: ErrorSettings {
                probability: 0.0,
                statuses: vec![500, 502, 503],
            },
            hang_probability: 0.0,
            drop_after_commit_probability: 0.0,
            rate_limit: RateLimitSettings {
                per_second: 20,
                burst: 10,
            },
            outage: false,
        }
    }

    #[test]
    fn 말이_안_되는_장애_설정은_보내기_전에_거절한다() {
        assert_eq!(healthy().validate(), Ok(()));
        let cases: Vec<(ChaosSettings, &str)> = vec![
            (
                ChaosSettings {
                    slow: SlowSettings {
                        probability: 1.5,
                        latency_ms: 3000,
                    },
                    ..healthy()
                },
                "slow.probability",
            ),
            (
                ChaosSettings {
                    latency_ms: LatencyRange { min: 200, max: 100 },
                    ..healthy()
                },
                "latency_ms.min",
            ),
            (
                ChaosSettings {
                    error: ErrorSettings {
                        probability: 0.1,
                        statuses: vec![404],
                    },
                    ..healthy()
                },
                "5xx",
            ),
            (
                ChaosSettings {
                    rate_limit: RateLimitSettings {
                        per_second: 0,
                        burst: 10,
                    },
                    ..healthy()
                },
                "rate_limit",
            ),
        ];
        for (settings, field) in cases {
            let reason = settings.validate().unwrap_err();
            assert!(reason.contains(field), "{reason}");
        }
    }
}
