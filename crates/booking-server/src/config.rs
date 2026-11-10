//! 서버 설정
//!
//! 환경 변수에서 읽는다 (≈ ASP.NET Core의 `IConfiguration` + 환경 변수 공급자). `.env` 파일은 `main`에서 먼저 읽어 둔다.
//!
//! | 환경 변수 | 기본값 |
//! |---|---|
//! | `DATABASE_URL` | (필수) |
//! | `BOOKING_LISTEN_ADDR` | `127.0.0.1:8080` |
//! | `BOOKING_DB_MAX_CONNECTIONS` | `10` |
//! | `BOOKING_WORKER_CONCURRENCY` | `4` |
//! | `BOOKING_WORKER_POLL_INTERVAL_MS` | `500` |
//! | `BOOKING_WORKER_LEASE_SECS` | `60` (업체 호출 최대 시간 + 5초 이상이어야 한다. 짧으면 서버가 시작하지 않는다) |
//! | `BOOKING_WORKER_UNKNOWN_RECHECK_SECS` | `30` |

use std::{net::SocketAddr, time::Duration};

use figment::{Figment, providers::Env};
use serde::Deserialize;

use crate::worker::WorkerConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub database_url: String,
    #[serde(default = "defaults::listen_addr")]
    pub listen_addr: SocketAddr,
    #[serde(default = "defaults::db_max_connections")]
    pub db_max_connections: u32,
    #[serde(default = "defaults::worker_concurrency")]
    pub worker_concurrency: usize,
    #[serde(default = "defaults::worker_poll_interval_ms")]
    pub worker_poll_interval_ms: u64,
    #[serde(default = "defaults::worker_lease_secs")]
    pub worker_lease_secs: u64,
    /// 결과 불명 예약을 업체에서 찾지 못했을 때 다시 조회하기까지의 간격
    #[serde(default = "defaults::worker_unknown_recheck_secs")]
    pub worker_unknown_recheck_secs: u64,
}

impl Config {
    /// `BOOKING_` 으로 시작하는 환경 변수(접두사를 뗀 소문자 이름 → 필드)와 `DATABASE_URL`을 읽는다.
    ///
    /// figment의 에러 값은 200바이트가 넘는다. Rust의 `Result`는 값을 그대로 담아 돌려주므로
    /// 큰 에러는 `Box`(힙에 두고 포인터만 넘김)로 감싸 반환 비용을 줄인다 (clippy `result_large_err`).
    pub fn load() -> Result<Self, Box<figment::Error>> {
        Figment::new()
            .merge(Env::prefixed("BOOKING_"))
            .merge(Env::raw().only(&["DATABASE_URL"]))
            .extract()
            .map_err(Box::new)
    }

    pub fn worker(&self) -> WorkerConfig {
        WorkerConfig {
            concurrency: self.worker_concurrency.max(1),
            poll_interval: Duration::from_millis(self.worker_poll_interval_ms),
            lease: Duration::from_secs(self.worker_lease_secs),
            unknown_recheck_interval: Duration::from_secs(self.worker_unknown_recheck_secs),
            // 재시도 간격은 설계 값(5초 → 최대 10분)을 그대로 쓴다
            ..WorkerConfig::default()
        }
    }
}

/// serde의 `#[serde(default = "...")]`는 함수 경로를 받는다. 기본값을 한곳에 모아 둔다.
mod defaults {
    use std::net::SocketAddr;

    pub fn listen_addr() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 8080))
    }

    pub fn db_max_connections() -> u32 {
        10
    }

    pub fn worker_concurrency() -> usize {
        4
    }

    pub fn worker_poll_interval_ms() -> u64 {
        500
    }

    pub fn worker_lease_secs() -> u64 {
        60
    }

    pub fn worker_unknown_recheck_secs() -> u64 {
        30
    }
}
