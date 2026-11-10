//! 예약 게이트웨이 서버: 예약 API + 워커
//!
//! 실행 파일(`main.rs`)은 설정을 읽고 이 라이브러리의 부품을 조립하기만 한다.
//! 부품을 라이브러리로 둔 이유는 통합 테스트(`tests/`)가 같은 부품을 직접 조립해 검증할 수 있게 하기 위해서다.
//!
//! | 모듈 | 역할 |
//! |---|---|
//! | [`config`] | 환경 변수에서 설정 읽기 |
//! | [`auth`] | API 키 발급 · 해시 · 요청 인증 |
//! | [`api`] | 예약 API (`/v1/bookings`)와 헬스 체크 |
//! | [`registry`] | 업체 ID → 어댑터 |
//! | [`worker`] | 작업 큐 처리 (업체에 예약 제출) |

pub mod api;
pub mod auth;
pub mod config;
pub mod registry;
pub mod worker;
