# 07. 개발 로드맵

단계마다 **실제로 돌려 볼 수 있는 결과물**이 나와야 한다. 각 단계에서 새로 배우는 Rust 개념도 함께 적었다.

## Phase 0. 준비 (진행 중)
- [x] Rust 설치 (rustup, stable)
- [x] git 저장소
- [x] 설계 문서 (이 폴더)
- [ ] Rust 기초 학습: The Rust Book 1~10장, Rustlings 일부

## Phase 1. 뼈대 + mock 업체 (alpha)
- [x] Cargo workspace, 크레이트 골격, `rust-toolchain.toml`, `cargo fmt` / `clippy` 설정
- [x] 품질 검사 스크립트(`scripts/check.sh`), Claude Code 훅, GitHub Actions CI, cargo-deny, cargo-shear
- [x] `docker-compose.yml` (postgres 18.6, otel-lgtm 0.40.0)
- [x] `telemetry` 크레이트: 콘솔 로그 초기화 (`LOG_FORMAT=json` 지원)
- [x] `mock-provider`: alpha의 자원 · 빈 시간 · 예약 생성/조회/취소 (메모리 저장), 멱등키, rate limit 헤더
- [x] 장애 주입 API와 프리셋, 테스트 28개 (단위 18 + HTTP 통합 10)

**완료 기준**: `curl`로 alpha에 예약을 만들 수 있고, `slow` 프리셋을 켜면 응답이 느려진다.
**배우는 것**: cargo, 모듈, struct/enum, serde, axum 핸들러 · 라우터 · State, `Arc<Mutex<_>>`, async/await 기초

## Phase 2. 예약 API + DB + 워커
- [x] `domain`: 예약 상태 머신 + 전이 단위 테스트 (8개 상태 × 11개 사건 전 조합 검사)
- [x] 마이그레이션, `db` 크레이트 (sqlx, 오프라인 쿼리 데이터 `.sqlx/`)
- [x] `POST/GET /v1/bookings`, Idempotency-Key, API 키 인증, Problem Details 에러
- [x] `jobs` 큐 + 워커 (`submit_booking`, lease 만료 회수 · fencing)
- [x] alpha 어댑터 (타임아웃만 적용한 단순 호출)
- [x] graceful shutdown (Ctrl+C를 누르면 처리 중인 작업을 마무리하고 종료)
- [x] `booking-server create-client` (API 키 발급), CI에 PostgreSQL 서비스와 `sqlx` 검사 추가

**완료 기준**: 예약을 요청하면 몇 초 뒤 `confirmed`가 된다. 같은 Idempotency-Key로 두 번 보내도 업체 예약은 1건이다.
**배우는 것**: `Result`와 `?`, thiserror, trait, sqlx 쿼리 매크로, 트랜잭션, `tokio::spawn`, CancellationToken

## Phase 3. 장애 대응 계층
- [x] `ProviderError` 분류 (`CircuitOpen` · `Throttled` 추가, 분류별 재시도 안전성 · 서킷 집계)
- [x] `ResilientProvider`: 재시도(백오프 + jitter), 시도 타임아웃, 전체 예산
- [x] rate limiter (governor), bulkhead (Semaphore), 업체 pause
- [x] 서킷 브레이커 직접 구현 + 단위 테스트
- [x] 결과 불명 처리 (`resolve_unknown`), 우리 쪽에서 막힌 작업은 시도 횟수를 쓰지 않고 미루기
- [x] 호출 기록기 (`provider_calls`, mpsc 배치 INSERT)
- [x] 장애 시나리오 통합 테스트 ([03-resilience.md §10](03-resilience.md#10-장애-시나리오와-기대-동작)): 정상 · 간헐적 5xx · 지연 폭증 · 호출 제한(2종) · 응답 유실 · 완전 장애 · 멱등키 없는 업체의 결과 불명

**완료 기준**: `flaky` / `slow` / `throttled` / `lossy` / `outage` 시나리오 테스트를 통과하고, 이중 예약이 0건이다.

뒤로 미룬 것 (다른 Phase에서 함께 하는 편이 자연스러운 것):
- 워커의 업체별 동시 처리 제한, "한 업체만 장애" 격리 시나리오 → 두 번째 업체가 생기는 Phase 6 ([D-19](decisions.md#d-19-워커의-업체별-동시-처리-제한은-두-번째-업체phase-6와-함께-만든다))
- "업체측 취소" 시나리오 → 대사 작업과 함께 Phase 6
- 장애 대응 메트릭(재시도 수, 서킷 상태, 버린 기록 수 등) → Phase 4. 지금은 로그와 `provider_calls`로 확인한다
- `provider_calls` 30일 보관 정리 작업 → 정리 작업(`jobs` 7일 보관)과 함께 Phase 7
**배우는 것**: 제네릭, trait 객체(`dyn`), `Arc`와 공유 상태, 채널, `tokio::time`, 테스트 작성법

## Phase 4. 관측
- [ ] OpenTelemetry 추적 · 메트릭 · 로그 → otel-lgtm
- [ ] `traceparent` 전파 (HTTP, 작업 큐 span link), mock-provider 계측
- [ ] 메트릭 구현 ([05 §4](05-observability-and-analytics.md#4-메트릭-목록))
- [ ] Grafana 대시보드 3종 + 알림 규칙 (코드로 관리)
- [ ] tokio-console 개발 모드

**완료 기준**: Grafana에서 업체별 p95를 보고, 느린 요청 하나를 골라 mock 내부까지 이어진 trace를 볼 수 있다.
**배우는 것**: tower Layer, tracing span과 `#[instrument]`, feature flag

## Phase 5. 관리자 API + 관리자 웹
- [ ] 관리자 API ([04 §3](04-api.md#3-관리자-api-adminapi)), utoipa로 OpenAPI 문서 생성
- [ ] 통계 API ([05 §7](05-observability-and-analytics.md#7-분석-쿼리-관리자-통계-api의-기반))
- [ ] React 앱: 대시보드, 예약 목록 · 상세, 업체 상세(차트), 작업 큐, 장애 주입
- [ ] 타입 자동 생성 파이프라인
- [ ] booking-server에서 정적 파일 제공

**완료 기준**: 관리자 웹에서 장애 주입 → 대시보드에서 서킷 Open 확인 → 복구 → dead 작업 재처리까지 화면만으로 할 수 있다.

## Phase 6. 두 번째 업체 + 운영 기능
- [ ] 멱등키 없는 업체 호출 직전 "보냄" 표시: 보낸 뒤 결과를 저장하기 전에 워커가 죽어도 다시 보내지 않고 `unknown`으로 ([03 §5](03-resilience.md#5-결과-불명-unknown-outcome-처리)의 알려진 한계)
- [ ] 워커의 업체별 동시 처리 제한 + "한 업체만 장애" 시나리오 테스트 ([D-19](decisions.md#d-19-워커의-업체별-동시-처리-제한은-두-번째-업체phase-6와-함께-만든다))
- [ ] mock beta + beta 어댑터 (시간대 변환, 200으로 오는 에러, 503 rate limit)
- [ ] 취소 흐름 (`cancel_booking`)
- [ ] 대사 작업 + 이슈 화면
- [ ] 정책 실시간 변경 (arc-swap)
- [ ] 빈 시간 캐시 + stale-if-error
- [ ] 적응형 rate limit (AIMD)
- [ ] 관리자 인증 · 권한

## Phase 7. 품질과 마무리
- [ ] 부하 테스트 (oha / goose) + 결과 기록
- [x] GitHub Actions CI: fmt, clippy, nextest, cargo-deny, cargo-shear (Phase 1에서 먼저 구축)
- [x] CI 확장: sqlx 오프라인 검사 (Phase 2에서 구축)
- [ ] CI 확장: 프론트 빌드 · 타입 생성 검사 (Phase 5)
- [ ] Dockerfile (다단계 빌드), docker-compose로 전체 실행
- [ ] 운영 문서 (장애 대응 런북)

## 이후 아이디어
- 여러 인스턴스로 확장: rate limit · 서킷 상태를 Redis로 공유
- 업체 웹훅 수신 (서명 검증, 중복 제거)
- `provider_calls` 파티셔닝 또는 TimescaleDB, 시간 단위 집계 테이블
- SSE로 관리자 대시보드에 실시간 푸시

## 학습 자료
- [The Rust Programming Language](https://doc.rust-lang.org/book/) ([한국어 번역](https://doc.rust-kr.org/))
- [Rustlings](https://github.com/rust-lang/rustlings): 작은 연습 문제 모음
- [Tokio 튜토리얼](https://tokio.rs/tokio/tutorial)
- 『Zero To Production in Rust』: 프로덕션 백엔드 전반
- [axum 예제 모음](https://github.com/tokio-rs/axum/tree/main/examples)
