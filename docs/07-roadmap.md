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
- 워커의 업체별 동시 처리 제한, "한 업체만 장애" 격리 시나리오 → 두 번째 업체가 생기는 Phase 6 ([D-19](decisions.md#d-19-워커의-업체별-동시-처리-제한은-두-번째-업체phase-6와-함께-만든다)) → Phase 6에서 완료
- "업체측 취소" 시나리오 → 대사 작업과 함께 Phase 6 → Phase 6에서 완료
- 장애 대응 메트릭(재시도 수, 서킷 상태, 버린 기록 수 등) → Phase 4. 지금은 로그와 `provider_calls`로 확인한다
- `provider_calls` 30일 보관 정리 작업 → 정리 작업(`jobs` 7일 보관)과 함께 Phase 7
**배우는 것**: 제네릭, trait 객체(`dyn`), `Arc`와 공유 상태, 채널, `tokio::time`, 테스트 작성법

## Phase 4. 관측
- [x] OpenTelemetry 추적 · 메트릭 · 로그 → otel-lgtm (`telemetry` 크레이트, OTLP/HTTP)
- [x] `traceparent` 전파 (HTTP, 작업 큐 span link), mock-provider 계측
- [x] 메트릭 구현 ([05 §4](05-observability-and-analytics.md#4-메트릭-목록)): Phase 3에서 미룬 장애 대응 메트릭 포함
- [x] 응답 헤더 `X-Trace-Id`, 에러 본문 `trace_id`, DB의 trace 컬럼(`booking_events` · `jobs` · `provider_calls`) 채우기
- [x] Grafana 대시보드 3종 + 알림 규칙 6개 (`observability/`, 컨테이너가 시작할 때 자동 등록)
- [x] tokio-console 개발 모드 (`tokio-console` feature)
- [x] 관측 통합 테스트: 메모리에 모은 span · 메트릭으로 trace 연결과 지표의 이름 · 라벨 · 버킷을 검사 (`tests/telemetry.rs`)

**완료 기준**: Grafana에서 업체별 p95를 보고, 느린 요청 하나를 골라 mock 내부까지 이어진 trace를 볼 수 있다.
→ Provider Overview 대시보드의 "호출 지연" 패널과 "느린 업체 호출" 표. 표의 행을 누르면 `job → provider.call → provider.attempt → HTTP → mock-provider → chaos.delay`까지 한 trace로 열린다.
**배우는 것**: tower Layer, tracing span과 `#[instrument]`, feature flag

뒤로 미룬 것:
- "대사에서 이중 예약 의심 발견" 알림 → 대사 작업을 만드는 Phase 6 → Phase 6에서 완료
- 관리자 웹 예약 상세의 trace 링크 → Phase 5 (DB에 trace ID는 이미 남는다) → Phase 5에서 완료
- 알림을 받을 곳(메일 · 메신저) 연결 → 배포 환경을 만들 때 (Phase 7 이후)
- 프로세스를 구분하는 리소스 속성(`service.instance.id`) → API와 워커를 따로 띄우거나 여러 대로 늘릴 때 ([05 §1](05-observability-and-analytics.md#1-전체-구성))

## Phase 5. 관리자 API + 관리자 웹
- [x] 관리자 API ([04 §3](04-api.md#3-관리자-api-adminapi)), utoipa로 OpenAPI 문서 생성 (`/openapi.json`, `/docs`, `booking-server openapi`)
  - 예약 목록 · 상세(상태 이력 · 호출 기록 · 작업), 실패한 예약 재시도와 dead 작업 재큐잉 ([D-26](decisions.md#d-26-관리자-재시도는-예약을-잠그고-dead-작업을-같은-행으로-되살리고-기한을-다시-잡고-보내기-전에-업체에서-먼저-찾는다): 다시 보내기 전에 업체에서 먼저 찾는다)
  - 업체 런타임 상태(서킷 · limiter 여유 · pause · 동시 실행 수)와 서킷 강제 조작 (`force_open` · `force_close` · `reset`)
  - 작업 목록 · 종류별 개수, mock 업체 장애 주입 중계 (`BOOKING_CHAOS_URL`, [D-27](decisions.md#d-27-장애-주입-중계는-booking_chaos_url을-설정할-때만-켠다))
- [x] 통계 API ([05 §7](05-observability-and-analytics.md#7-분석-쿼리-관리자-통계-api의-기반)): 구간별 시계열(빈 구간 0으로 채움), 업체별 요약
- [x] React 앱: 대시보드, 예약 목록 · 상세, 업체 상세(차트), 작업 큐, 장애 주입 (`web/admin`, [06 §7](06-admin-frontend.md#7-구현-메모-phase-5))
- [x] 타입 자동 생성 파이프라인 (`pnpm gen`, `scripts/check.sh api` · `web`, CI의 `api` · `web` job)
- [x] booking-server에서 정적 파일 제공 (`/admin`, 없는 화면 경로는 index.html)
- [x] 관리자 API 통합 테스트 (`tests/admin.rs` 16개), db 쿼리 테스트, 서킷 강제 조작 단위 테스트

**완료 기준**: 관리자 웹에서 장애 주입 → 대시보드에서 서킷 Open 확인 → 복구 → dead 작업 재처리까지 화면만으로 할 수 있다.
→ 장애 주입 화면의 "완전 장애" → 대시보드 카드가 "열림 (차단)", 작업이 "미룸"으로 쌓임 → 확정 기한이 지나면 실패 · dead → "정상"으로 복구 → 작업 큐의 "다시 넣기"(또는 예약 상세의 "재시도") → 워커가 업체에서 먼저 찾아본 뒤(`find_booking`) 제출(`create_booking`)해 확정. 실제 서버와 mock 업체로 끝까지 돌려 확인했다.
**배우는 것**: utoipa(속성 매크로로 문서 만들기), `#[cfg_attr(feature, derive(..))]`, `From` 변환과 `?`, 트랜잭션 안의 행 잠금(`FOR UPDATE`), 정적 파일 제공(tower-http). React: 컴포넌트와 훅, TanStack Query(조회 · 변경 · 무한 목록), React Router, `lazy`로 화면 나눠 내려받기

뒤로 미룬 것:
- 업체 정책 편집(`PUT /admin/api/providers/{id}/policy`)과 편집 폼 → 정책 실시간 변경과 함께 Phase 6 ([D-28](decisions.md#d-28-업체-정책-편집은-정책-실시간-반영phase-6과-함께-만든다)). 지금은 읽기 전용 → Phase 6에서 완료
- 대사 이슈 API · 화면, 대시보드의 미해결 이슈 수 → 대사 작업과 함께 Phase 6 → Phase 6에서 완료
- `cancel_failed` 예약 재시도 → 취소 흐름과 함께 Phase 6 → Phase 6에서 완료
- 관리자 인증 · 권한, 조작한 관리자 이름을 이력에 남기기(지금은 `admin`) → Phase 6 → Phase 6에서 완료

## Phase 6. 두 번째 업체 + 운영 기능
- [x] 멱등키 없는 업체 호출 직전 "보냄" 표시: 보낸 뒤 결과를 저장하기 전에 워커가 죽어도 다시 보내지 않고 `unknown`으로 ([03 §5](03-resilience.md#5-결과-불명-unknown-outcome-처리)의 알려진 한계, [D-31](decisions.md#d-31-멱등키-없는-업체에는-보내기-직전에-보냄-표시를-커밋한다-jobssent_at))
- [x] 워커의 업체별 동시 처리 제한 + "한 업체만 장애" 시나리오 테스트 ([D-19](decisions.md#d-19-워커의-업체별-동시-처리-제한은-두-번째-업체phase-6와-함께-만든다) → [D-30](decisions.md#d-30-워커는-업체마다-루프를-따로-두고-작업에-업체를-적는다-jobsprovider_id): 업체마다 워커 루프, `jobs.provider_id`)
- [x] mock beta + beta 어댑터 (시간대 변환, 200으로 오는 에러, 503 rate limit) + 실제 mock에 붙이는 어댑터 계약 테스트 ([04 §4](04-api.md#4-mock-provider-api-포트-9090))
- [x] 취소 흐름 (`cancel_booking`) + `cancel_failed` 관리자 재시도 ([D-32](decisions.md#d-32-취소는-결과를-모르는-실패-뒤에도-다시-보내고-cancel_failed는-관리자가-다시-시도한다))
- [x] 대사 작업 + 이슈 API · 화면 (대시보드의 미해결 이슈 수 포함), 이중 예약 의심 알림, `failed → confirmed` 전이 ([D-33](decisions.md#d-33-대사는-업체별-작업-체인으로-돌고-자동으로-바로잡을-것과-사람에게-넘길-것을-나눈다))
- [x] 정책 실시간 변경 (arc-swap) + 관리자 정책 편집 API · 폼 ([D-28](decisions.md#d-28-업체-정책-편집은-정책-실시간-반영phase-6과-함께-만든다) → [D-34](decisions.md#d-34-정책은-실행-중에-통째로-바꾸고arcswap-호출-하나는-시작할-때의-정책으로-끝까지-간다))
- [x] 빈 시간 캐시 + stale-if-error (`GET /v1/providers`, `GET /v1/providers/{provider}/availability`, [D-37](decisions.md#d-37-빈-시간-캐시-새것으로-보는-시간은-정책에서-업체가-실패하면-5분까지-예전-값을-준다))
- [x] 적응형 rate limit (AIMD) + `provider.rate_limit.allowed` 지표 · 대시보드 ([D-35](decisions.md#d-35-적응형-rate-limitaimd-한도-초과면-절반으로-조용하면-상한의-10씩-올린다))
- [x] 관리자 인증 · 권한 (`create-admin`, 로그인 화면, viewer · operator · admin, 감사 로그 화면, [D-36](decisions.md#d-36-관리자-인증-argon2-비밀번호--db-세션-쿠키-권한은-추출기-타입으로-조작은-감사-로그로))

**완료 기준**: 멱등키 없는 beta에 응답 유실을 주입해도 이중 예약이 0건이고, 업체측 취소를 대사가 찾아 바로잡고, 한 업체가 느려져도 다른 업체 예약은 밀리지 않는다. 관리자는 로그인해 역할에 맞는 조작만 할 수 있고, 정책을 바꾸면 재시작 없이 반영된다.
→ 실제 서버와 mock 업체로 끝까지 돌려 확인했다: beta 응답 유실 50%에서 예약 6건 중 3건이 결과 불명을 거쳐 모두 확정(업체 예약 참조값 중복 0건), 클라이언트 취소 → 업체에서도 취소, mock에서 업체측 취소 + 같은 참조값으로 한 건 더 → "지금 대사" → `cancelled`(actor `reconciler`) + 이중 예약 의심 이슈 → 처리 완료(`admin:boss`), 정책 PUT(초당 20) → 즉시 반영 → mock alpha를 초당 2회로 조이자 AIMD가 20 → 10 → 5 → 2로 줄이고 예약은 모두 확정, beta 장애 중 빈 시간 조회는 예전 값을 `stale: true`로(캐시도 없으면 503), viewer는 조작이 403. 관리자 웹에서도 로그인 · 대시보드 · 대사 이슈 · 정책 편집 → 감사 로그를 확인했다.
**배우는 것**: `ArcSwap`(잠금 없이 통째로 바꾸는 공유 값), `spawn_blocking`(CPU 작업을 async 밖으로), axum 추출기로 권한 확인(`FromRequestParts`), `derive(PartialOrd, Ord)`, `JoinSet`, moka 캐시, chrono 시간대 변환, serde `rename_all = "camelCase"`, 시각을 인자로 받는 상태 머신(AIMD), PostgreSQL 부분 유니크 인덱스 + `ON CONFLICT`(인덱스의 `WHERE`까지 맞춰 적기) · `NULLS NOT DISTINCT`. React: 로그인 흐름(세션 쿼리 + 전역 401 처리), 역할에 따라 버튼 끄기, 편집 폼

뒤로 미룬 것:
- 정책 변경을 다른 프로세스에 알리기 (DB 주기적 재조회 또는 `LISTEN/NOTIFY`) → API와 워커를 따로 띄우거나 여러 대로 늘릴 때 ([D-34](decisions.md#d-34-정책은-실행-중에-통째로-바꾸고arcswap-호출-하나는-시작할-때의-정책으로-끝까지-간다))
- 다시 로그인하지 않는 관리자의 만료된 세션 정리 → Phase 7의 정리 작업과 함께 (로그인할 때 그 관리자의 만료 세션은 지운다)
- 로그인 시도 횟수 제한, 관리자 계정 관리 화면(만들기 · 역할 변경 · 비밀번호 변경) → Phase 7 이후. 지금은 `create-admin` 명령으로만 만든다
- 빈 시간 조회에서 같은 칸의 동시 첫 요청 합치기(single-flight) → 필요해지면 ([D-37](decisions.md#d-37-빈-시간-캐시-새것으로-보는-시간은-정책에서-업체가-실패하면-5분까지-예전-값을-준다))
- 대사 범위(지난 24시간 ~ 앞으로 7일) 밖의 불일치 → 필요해지면 범위를 정책으로

## Phase 7. 품질과 마무리
- [ ] 부하 테스트 (oha / goose) + 결과 기록
- [ ] 보관 정리 작업: `provider_calls` 30일, 끝난 `jobs` 7일, 만료된 관리자 세션 ([02 §6](02-domain-and-data.md#6-보관-정책))
- [x] GitHub Actions CI: fmt, clippy, nextest, cargo-deny, cargo-shear (Phase 1에서 먼저 구축)
- [x] CI 확장: sqlx 오프라인 검사 (Phase 2에서 구축)
- [x] CI 확장: 프론트 빌드 · 타입 생성 검사 (Phase 5에서 구축: `api` · `web` job)
- [ ] 관리자 화면의 조회 요청(폴링)이 Tempo의 trace 목록을 채우지 않게 하기 (조회 요청만 샘플링에서 빼기 등, [05 §3](05-observability-and-analytics.md#3-추적-traces))
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
