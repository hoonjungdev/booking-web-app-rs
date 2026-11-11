//! 서버 설정
//!
//! 환경 변수에서 읽는다 (≈ ASP.NET Core의 `IConfiguration` + 환경 변수 공급자). `.env` 파일은 `main`에서 먼저 읽어 둔다.
//!
//! | 환경 변수 | 기본값 |
//! |---|---|
//! | `DATABASE_URL` | (필수) |
//! | `BOOKING_LISTEN_ADDR` | `127.0.0.1:8080` |
//! | `BOOKING_DB_MAX_CONNECTIONS` | `10` |
//! | `BOOKING_WORKER_CONCURRENCY` | `4` (업체 하나당 동시 처리 수. 업체마다 이만큼의 워커 루프) |
//! | `BOOKING_WORKER_POLL_INTERVAL_MS` | `500` |
//! | `BOOKING_WORKER_LEASE_SECS` | `60` (업체 호출 최대 시간 + 5초 이상이어야 한다. 짧으면 서버가 시작하지 않는다) |
//! | `BOOKING_WORKER_UNKNOWN_RECHECK_SECS` | `30` |
//! | `BOOKING_RECONCILE_INTERVAL_SECS` | `600` (업체마다 대사를 도는 간격) |
//! | `BOOKING_ADMIN_SESSION_HOURS` | `12` (관리자 로그인 세션 유지 시간) |
//! | `BOOKING_ADMIN_COOKIE_SECURE` | `true` (세션 쿠키에 `Secure`. `localhost`가 아닌 http 주소로 쓸 때만 끈다) |
//! | `BOOKING_ADMIN_WEB_DIR` | `web/admin/dist` (관리자 웹 빌드 결과. 없으면 `/admin` 화면 없이 API만 연다) |
//! | `BOOKING_GRAFANA_URL` | `http://localhost:3000` (관리자 화면의 trace 링크. 빈 값이면 링크를 보여 주지 않는다) |
//! | `BOOKING_CHAOS_URL` | 없음 (mock 업체 주소. 설정하면 관리자 화면에서 장애를 주입할 수 있다. 개발 환경 전용) |

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use figment::{Figment, providers::Env};
use serde::Deserialize;

use crate::{admin_auth::AdminAuthConfig, worker::WorkerConfig};

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
    /// 업체마다 대사를 도는 간격
    #[serde(default = "defaults::reconcile_interval_secs")]
    pub reconcile_interval_secs: u64,
    /// 관리자 로그인 세션 유지 시간
    #[serde(default = "defaults::admin_session_hours")]
    pub admin_session_hours: u64,
    /// 관리자 세션 쿠키에 `Secure`를 붙인다
    #[serde(default = "defaults::admin_cookie_secure")]
    pub admin_cookie_secure: bool,
    /// 관리자 웹 빌드 결과(`pnpm build`)가 있는 폴더. 서버를 실행한 위치 기준이다.
    #[serde(default = "defaults::admin_web_dir")]
    pub admin_web_dir: PathBuf,
    /// Grafana 주소. 관리자 화면이 trace ID를 Grafana(Tempo) 링크로 보여 줄 때 쓴다.
    #[serde(default = "defaults::grafana_url")]
    pub grafana_url: String,
    /// mock 업체 주소 (예: `http://127.0.0.1:9090`). 장애 주입 API를 중계할 곳이다.
    #[serde(default)]
    pub chaos_url: Option<String>,
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
            reconcile_interval: Duration::from_secs(self.reconcile_interval_secs.max(1)),
            // 재시도 간격은 설계 값(5초 → 최대 10분)을 그대로 쓴다
            ..WorkerConfig::default()
        }
    }

    pub fn admin_auth(&self) -> AdminAuthConfig {
        AdminAuthConfig {
            session_ttl: Duration::from_secs(self.admin_session_hours.max(1) * 3600),
            cookie_secure: self.admin_cookie_secure,
        }
    }

    /// trace 링크에 쓸 Grafana 주소. 빈 값이면 `None` (링크를 보여 주지 않는다).
    pub fn grafana_url(&self) -> Option<String> {
        let url = self.grafana_url.trim().trim_end_matches('/');
        (!url.is_empty()).then(|| url.to_owned())
    }

    /// 장애 주입 API를 중계할 mock 업체 주소. 설정하지 않았거나 빈 값이면 `None`.
    pub fn chaos_url(&self) -> Option<&str> {
        self.chaos_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
    }
}

/// serde의 `#[serde(default = "...")]`는 함수 경로를 받는다. 기본값을 한곳에 모아 둔다.
mod defaults {
    use std::{net::SocketAddr, path::PathBuf};

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

    pub fn reconcile_interval_secs() -> u64 {
        600
    }

    pub fn admin_session_hours() -> u64 {
        12
    }

    pub fn admin_cookie_secure() -> bool {
        true
    }

    pub fn admin_web_dir() -> PathBuf {
        PathBuf::from("web/admin/dist")
    }

    pub fn grafana_url() -> String {
        "http://localhost:3000".to_owned()
    }
}
