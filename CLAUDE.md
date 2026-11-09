# CLAUDE.md

외부 예약 업체 API와 연동하는 견고한 예약 게이트웨이 서비스. **사용자가 Rust를 배우면서 함께 만드는 학습 프로젝트**다.

## 작업 방식

- 모든 설명, 주석, 커밋 메시지는 한국어로 쓴다. 코드 식별자와 기술 용어는 원문 그대로 둔다.
- 사용자는 Rust 초보이고 C#/.NET 경험이 많으며 React는 초급이다.
  - 새 Rust 개념이 처음 나오면 C# 대응 개념으로 짧게 설명한다 (대응표: `docs/01-architecture.md` §8).
  - 한 번에 너무 많은 개념을 넣지 않는다. 현재 Phase에 맞는 수준으로 코드를 쓰고, 고급 기법(매크로 작성, 복잡한 lifetime, unsafe)은 꼭 필요할 때만 쓰고 이유를 설명한다.
  - 사용자가 직접 작성해 보고 싶다고 하면 정답 대신 힌트와 방향을 먼저 준다.
- 구현은 `docs/07-roadmap.md`의 Phase 순서를 따른다. 항목을 끝내면 체크박스를 갱신한다.
- 설계와 다르게 구현해야 하면 먼저 사용자와 상의하고, 결정되면 `docs/decisions.md`에 새 항목을 추가하고 관련 문서를 고친다. 기존 결정은 지우지 않고 `대체됨 (→ D-xx)`으로 표시한다.
- 커밋은 사용자가 요청할 때만 한다.

## 설계 문서

구현 전에 해당 문서를 먼저 읽는다. 문서가 기준이다.

| 문서 | 내용 |
|---|---|
| `docs/01-architecture.md` | 구성, 크레이트 구조와 의존 방향, 기술 스택 |
| `docs/02-domain-and-data.md` | 예약 상태 머신, 멱등성, DB 스키마, 작업 큐 |
| `docs/03-resilience.md` | 실패 분류, 장애 대응 파이프라인, 결과 불명 처리, 장애 시나리오 |
| `docs/04-api.md` | 예약 · 관리자 · mock 업체 API |
| `docs/05-observability-and-analytics.md` | 추적 · 메트릭 · 로그, 통계 쿼리, 알림 |
| `docs/06-admin-frontend.md` | React 관리자 웹 |
| `docs/07-roadmap.md` | 단계별 계획과 진행 상황 |
| `docs/decisions.md` | 설계 결정 기록 |

## 구조 규칙

- 크레이트 의존 방향을 지킨다: `domain` ← `providers`, `db` ← `booking-server`. `telemetry`는 서버와 mock이 함께 쓴다.
- `domain`에는 I/O(DB, HTTP, 파일, 시계 직접 호출)를 넣지 않는다. 순수 로직과 타입만 둔다.
- `mock-provider`는 `domain`을 포함한 우리 크레이트에 의존하지 않는다 (진짜 외부 업체처럼 독립).
- 어댑터는 HTTP ↔ 도메인 변환과 응답 분류만 한다. 재시도 · 서킷 · rate limit은 `ResilientProvider`가 맡는다.

## 반드시 지킬 불변 조건

- **이중 예약 0건**: 멱등키를 지원하지 않는 업체에는 타임아웃 · 5xx 뒤에 생성 요청을 그냥 재시도하지 않는다. `unknown` → 조회 절차를 거친다 (`docs/03-resilience.md` §5).
- **유실 0건**: 예약 저장, 상태 이력, 작업 등록은 한 트랜잭션으로 묶는다.
- **상태 전이**는 `domain`의 전이 함수를 거치고, SQL은 `WHERE id = $1 AND status = $expected`로 낙관적 동시성을 지킨다. 전이할 때마다 `booking_events`에 기록한다.
- **관측이 서비스를 느리게 하면 안 된다**: 호출 기록은 채널로 비동기 배치 저장하고, 채널이 가득 차면 버린다 (막지 않는다).

## 코딩 규칙

### Rust
- 에러: 라이브러리 크레이트는 `thiserror`로 타입을 정의하고, 실행 파일의 최상위에서만 `anyhow`를 쓴다.
- 테스트가 아닌 코드에서 `unwrap()` / `expect()`를 쓰지 않는다. 정말 불가능한 경우만 `expect("이유")`로 이유를 남긴다.
- 출력은 `println!` 대신 `tracing` 매크로(`info!`, `warn!` …)에 구조화 필드로 남긴다.
- 고객 이름 · 전화번호 같은 개인정보는 로그와 span 속성에 넣지 않는다.
- 메트릭 라벨에 `booking_id`처럼 값이 끝없이 늘어나는 것을 넣지 않는다.
- 시각은 UTC(`DateTime<Utc>`, DB는 `timestamptz`), ID는 UUID v7.
- DB 쿼리는 sqlx의 `query!` / `query_as!` 매크로로 컴파일 시점에 검사한다. 쿼리를 바꾸면 `cargo sqlx prepare --workspace`로 `.sqlx/`를 갱신해서 함께 커밋한다.
- 이미 적용된 마이그레이션 파일은 수정하지 않는다. 바꿀 것이 있으면 새 마이그레이션을 추가한다.
- 코드를 바꾼 뒤에는 `cargo fmt`와 `cargo clippy`를 통과시킨다.

### 관리자 웹 (`web/admin`)
- 패키지 매니저는 pnpm.
- 서버 데이터는 TanStack Query 훅으로만 가져온다 (`useEffect` 안에서 fetch하지 않는다).
- `src/api/schema.d.ts`는 자동 생성 파일이므로 직접 고치지 않는다. 백엔드 API가 바뀌면 OpenAPI를 다시 생성한다.

## 명령어

Phase 1에서 workspace를 만든 뒤 실제 명령을 확인하고 이 절을 갱신한다.

```bash
cargo fmt --all                                   # 포맷
cargo clippy --workspace --all-targets -- -D warnings   # 린트
cargo test --workspace                            # 테스트
docker compose up -d                              # postgres, otel-lgtm
```

## 로컬 포트

| 서비스 | 포트 |
|---|---|
| booking-server | 8080 |
| mock-provider | 9090 |
| PostgreSQL | 5432 |
| Grafana | 3000 |
| OTLP | 4317 (gRPC), 4318 (HTTP) |
| Vite 개발 서버 | 5173 |
