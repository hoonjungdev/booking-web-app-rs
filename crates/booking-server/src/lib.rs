//! 예약 게이트웨이 서버: 예약 API + 관리자 API + 워커
//!
//! 실행 파일(`main.rs`)은 설정을 읽고 이 라이브러리의 부품을 조립하기만 한다.
//! 부품을 라이브러리로 둔 이유는 통합 테스트(`tests/`)가 같은 부품을 직접 조립해 검증할 수 있게 하기 위해서다.
//!
//! | 모듈 | 역할 |
//! |---|---|
//! | [`config`] | 환경 변수에서 설정 읽기 |
//! | [`auth`] | API 키 발급 · 해시 · 요청 인증 |
//! | [`api`] | 예약 API (`/v1`), 관리자 API (`/admin/api`), API 문서 (`/openapi.json`, `/docs`), 헬스 체크 |
//! | [`web`] | 관리자 웹 정적 파일 (`/admin`) |
//! | [`chaos`] | mock 업체 장애 주입 API 중계 (개발 환경 전용) |
//! | [`registry`] | 업체 ID → 장애 대응 파이프라인으로 감싼 업체 (`ResilientProvider`) |
//! | [`worker`] | 작업 큐 처리 (업체에 예약 제출, 결과 불명 예약 확인) |
//! | [`recorder`] | 업체 호출 기록을 모아 `provider_calls`에 저장 |
//! | [`metrics`] | 예약 · 작업 큐 · 업체 상태 지표 |
//! | [`trace`] | 접수 요청과 작업 처리를 trace로 잇기 |

pub mod api;
pub mod auth;
pub mod chaos;
pub mod config;
pub mod metrics;
pub mod recorder;
pub mod registry;
pub mod trace;
pub mod web;
pub mod worker;

/// `RUST_LOG`가 없을 때 쓰는 로그 필터. 콘솔 로그와 OpenTelemetry로 내보내는 span · 로그에 함께 적용된다.
///
/// 우리 크레이트는 debug까지, 그 밖의 라이브러리는 info부터 남긴다. `sqlx`는 쿼리마다 로그를 남기므로 warn으로 줄인다.
/// 통합 테스트(`tests/telemetry.rs`)도 이 값을 써서, 실제 서버 설정에서 보이는 span을 검증한다.
pub const DEFAULT_LOG_FILTER: &str =
    "info,booking_server=debug,providers=debug,telemetry=debug,sqlx=warn";
