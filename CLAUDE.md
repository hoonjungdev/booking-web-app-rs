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
- DB 쿼리는 `db` 크레이트에 두고 sqlx의 `query!` / `query_as!` 매크로로 컴파일 시점에 검사한다. 쿼리를 바꾸면 `cargo sqlx prepare --workspace -- --all-targets`로 `.sqlx/`를 갱신해서 함께 커밋한다 (`scripts/check.sh`는 `.sqlx/`로 오프라인 검사하므로, 갱신하지 않으면 컴파일 에러가 난다). 테스트에서 준비용으로만 쓰는 SQL은 검사 없는 `sqlx::query(...)`를 써도 된다.
- 예약 상태는 `db::bookings::transition`(내부에서 `BookingStatus::apply` 호출)으로만 바꾼다. `bookings.status`를 직접 UPDATE하지 않는다 (테스트 준비 코드 제외).
- 이미 적용된 마이그레이션 파일은 수정하지 않는다. 바꿀 것이 있으면 새 마이그레이션을 추가한다.
- 통합 테스트(`tests/`)의 헬퍼 함수에서는 `unwrap()` 대신 `expect("이유")`를 쓴다. clippy의 테스트 예외는 `#[test]` 함수 안에만 적용되기 때문이다.
- 코드를 바꾼 뒤에는 `cargo fmt`와 `cargo clippy`를 통과시킨다.

### 관리자 웹 (`web/admin`)
- 패키지 매니저는 pnpm.
- 서버 데이터는 TanStack Query 훅으로만 가져온다 (`useEffect` 안에서 fetch하지 않는다).
- `src/api/schema.d.ts`는 자동 생성 파일이므로 직접 고치지 않는다. 백엔드 API가 바뀌면 OpenAPI를 다시 생성한다.

## 품질 검사와 개발 루프

검사는 `scripts/check.sh` 한 곳에 정의되어 있고, 로컬 · Claude Code 훅 · CI가 모두 이 스크립트를 쓴다. 검사 내용을 바꿀 때는 이 스크립트만 고친다.

```bash
scripts/check.sh          # 전체: fmt → clippy → test → sqlx → deny → shear (CI와 동일)
scripts/check.sh fast     # fmt + clippy (Stop 훅이 쓰는 빠른 검사, DB 불필요)
scripts/check.sh test     # 단계별 실행: fmt | clippy | test | sqlx | deny | shear
cargo fmt --all           # 포맷 자동 수정
```

| 단계 | 도구 | 잡는 것 |
|---|---|---|
| fmt | rustfmt | 포맷 (`rustfmt.toml`) |
| clippy | clippy `-D warnings` | 버그 패턴, 그리고 CLAUDE.md 규칙 위반: `unwrap`, `println!`, `dbg!`, `todo!` 등 (`Cargo.toml`의 `[workspace.lints]`) |
| test | cargo-nextest + doctest | 테스트 실패. DB 테스트(`#[sqlx::test]`)가 있어서 PostgreSQL이 떠 있어야 한다 |
| sqlx | sqlx-cli | 마이그레이션 적용 후 `.sqlx/`가 스키마와 맞는지 (`cargo sqlx prepare --check`) |
| deny | cargo-deny | 보안 권고, 허용하지 않은 라이선스, 출처 불명 의존성 (`deny.toml`) |
| shear | cargo-shear | `Cargo.toml`에 선언했지만 쓰지 않는 의존성 |

Claude Code 훅 (`.claude/settings.json`, `.claude/hooks/`):
- **PostToolUse** (Edit · Write · MultiEdit): 수정한 `.rs` 파일을 즉시 rustfmt로 정리한다. 그래서 편집 직후 파일 내용이 바뀔 수 있다. 이어서 같은 부분을 고칠 때는 파일을 다시 읽는다. `mod x;`를 선언한 파일을 하위 파일보다 먼저 쓰면 그 시점의 rustfmt는 실패하고 조용히 넘어간다. 그런 포맷 누락은 Stop 훅의 fmt 검사가 잡으므로 `cargo fmt --all`로 정리한다.
- **Stop**: 응답을 끝내기 전에 `scripts/check.sh fast`를 돌린다. 실패하면 오류가 돌아오므로 **고친 뒤에** 마무리한다. 두 번째에도 실패하면 막지 않고 사용자에게 경고만 표시된다.
- Stop 훅은 빠른 검사만 한다. 기능을 끝냈다고 보고하기 전에는 `scripts/check.sh`(전체)를 직접 돌려 통과를 확인한다.

규칙:
- 린트를 끄는 `#[allow(...)]`는 꼭 필요할 때만, 가장 좁은 범위(함수 · 문장)에 붙이고 이유를 주석으로 남긴다. `[workspace.lints]`를 느슨하게 바꾸는 것은 사용자와 상의한다.
- 의존성을 추가하면 `Cargo.lock`도 함께 커밋한다 (모든 검사가 `--locked`로 돈다).
- 컴파일러 버전은 `rust-toolchain.toml`에 고정되어 있다. 올릴 때는 버전을 바꾸고 `scripts/check.sh` 통과를 확인한 뒤 별도 커밋으로 올린다.
- 필요한 도구: `brew install cargo-nextest cargo-deny cargo-shear actionlint sqlx-cli` (sqlx-cli는 sqlx 크레이트와 같은 0.9.0)

## 그 밖의 명령어

```bash
cp .env.example .env                       # 처음 한 번. DATABASE_URL 등 (.env는 커밋하지 않음)
docker compose up -d --wait                # postgres(:5432), otel-lgtm(Grafana :3000, OTLP :4317/4318)
docker compose down                        # 정지 (-v를 붙이면 DB 데이터까지 삭제)
cargo run -p mock-provider                 # mock 업체 서버 (:9090). RUST_LOG=debug로 장애 주입 로그 확인
cargo run -p booking-server -- serve       # 예약 서버 (:8080, API + 워커). 시작할 때 마이그레이션 적용
cargo run -p booking-server -- create-client --name <이름>   # API 키 발급 (stdout에 한 번만 출력)
sqlx migrate add <설명>                     # 새 마이그레이션 파일 (migrations/)
cargo sqlx prepare --workspace -- --all-targets   # 쿼리를 바꾼 뒤 .sqlx/ 갱신 (DB 필요)
actionlint                                 # .github/workflows 검사
```

- 셸 환경에 따라 `cargo`가 PATH에 없을 수 있다. 그럴 때는 `. "$HOME/.cargo/env"`를 먼저 실행한다.
- `cargo nextest`를 직접 돌릴 때는 `DATABASE_URL`이 환경 변수로 있어야 한다 (`set -a; . ./.env; set +a`). `scripts/check.sh`는 기본값을 알아서 넣는다.
- mock 업체의 장애 설정은 `curl -X POST localhost:9090/_chaos/alpha/preset/<healthy|flaky|slow|throttled|lossy|outage>`로 바꾼다.

## 로컬 포트

| 서비스 | 포트 |
|---|---|
| booking-server | 8080 |
| mock-provider | 9090 |
| PostgreSQL | 5432 |
| Grafana | 3000 |
| OTLP | 4317 (gRPC), 4318 (HTTP) |
| Vite 개발 서버 | 5173 |
