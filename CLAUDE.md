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
- OpenTelemetry SDK와 exporter는 `telemetry` 크레이트에만 둔다. `providers` · `booking-server`의 계측 코드는 `tracing` 매크로와 `opentelemetry`(API) 계측기만 쓰고, `db` · `domain`은 OpenTelemetry를 모른다 (trace ID는 문자열 · JSON으로 받아 저장만 한다). → `docs/decisions.md` D-20

## 반드시 지킬 불변 조건

- **이중 예약 0건**: 멱등키를 지원하지 않는 업체에는 타임아웃 · 5xx 뒤에 생성 요청을 그냥 재시도하지 않는다. `unknown` → 조회 절차를 거친다 (`docs/03-resilience.md` §5). 그런 업체에 생성 요청을 보내기 직전에는 `jobs::mark_sent`로 "보냄" 표시를 커밋하고, 표시가 남은 채 다시 가져온 작업은 보내지 않고 `unknown`으로 넘긴다 (`docs/decisions.md` D-31). 업체 호출 한 번의 최대 시간은 워커 lease보다 짧아야 한다 (정책 변경도 검사한다).
- **유실 0건**: 예약 저장, 상태 이력, 작업 등록은 한 트랜잭션으로 묶는다.
- **상태 전이**는 `domain`의 전이 함수를 거치고, SQL은 `WHERE id = $1 AND status = $expected`로 낙관적 동시성을 지킨다. 전이할 때마다 `booking_events`에 기록한다.
- **관측이 서비스를 느리게 하면 안 된다**: 호출 기록은 채널로 비동기 배치 저장하고, 채널이 가득 차면 버린다 (막지 않는다). 추적 · 메트릭 · 로그도 SDK의 전용 스레드가 내보낸다. 요청 처리 경로와 지표 콜백에서 DB · 네트워크를 기다리지 않는다.

## 코딩 규칙

### Rust
- 에러: 라이브러리 크레이트는 `thiserror`로 타입을 정의하고, 실행 파일의 최상위에서만 `anyhow`를 쓴다.
- 테스트가 아닌 코드에서 `unwrap()` / `expect()`를 쓰지 않는다. 정말 불가능한 경우만 `expect("이유")`로 이유를 남긴다.
- 출력은 `println!` 대신 `tracing` 매크로(`info!`, `warn!` …)에 구조화 필드로 남긴다.
- 고객 이름 · 전화번호 같은 개인정보는 로그와 span 속성에 넣지 않는다. 업체가 보낸 글자(오류 본문 등)도 같다: 요청 값을 되돌려 줄 수 있으므로 오류 메시지(`Display`) · 로그 · span에 넣지 않고, `ProviderError::detail()`로 꺼내 호출 기록(`provider_calls.error_detail`)에만 남긴다.
- 메트릭 라벨에 `booking_id`처럼 값이 끝없이 늘어나는 것을 넣지 않는다. span 이름도 같다 (`POST /v1/bookings/{id}`처럼 틀을 쓰고, ID는 속성에 넣는다).
- span을 여는 `#[tracing::instrument]`에는 `skip_all`을 붙이고 남길 값만 `fields(..)`에 고른다 (인자에 고객 정보가 있을 수 있다). 워커가 할 일이 없을 때도 계속 부르는 함수(`db::jobs::claim_next`)에는 span을 열지 않는다.
- 지표를 추가하거나 이름 · 라벨을 바꾸면 `docs/05-observability-and-analytics.md` §4 표, `observability/`의 대시보드 · 알림 규칙, `crates/booking-server/tests/telemetry.rs`를 함께 고친다. 초 단위 히스토그램에는 버킷 경계를 직접 지정한다 (기본 버킷은 밀리초 기준).
- tracing layer에 per-layer filter(`Layer::with_filter`)를 걸지 않는다. sqlx의 `tracing::enabled!` 검사와 만나 span · 로그가 무작위로 빠진다 (`docs/decisions.md` D-22). 필터는 전역으로 건다 (`telemetry::filter`): 로그는 `RUST_LOG`를 따르고, span은 info 이상이면 항상 만든다. 그래서 추적에 남길 span은 info 수준(기본값)으로 만든다.
- 응답을 받은 "뒤에" 기록하는 코드는 타임아웃으로 Future가 버려지면 실행되지 않는다. 버려져도 남겨야 하는 기록(걸린 시간, 오류 표시)은 `Drop`에 둔다 (`telemetry::http::RequestRecord`, `providers::trace::Pending`).
- 시각은 UTC(`DateTime<Utc>`, DB는 `timestamptz`), ID는 UUID v7.
- DB 쿼리는 `db` 크레이트에 두고 sqlx의 `query!` / `query_as!` 매크로로 컴파일 시점에 검사한다. 쿼리를 바꾸면 `cargo sqlx prepare --workspace -- --all-targets`로 `.sqlx/`를 갱신해서 함께 커밋한다 (`scripts/check.sh`는 `.sqlx/`로 오프라인 검사하므로, 갱신하지 않으면 컴파일 에러가 난다). 테스트에서 준비용으로만 쓰는 SQL은 검사 없는 `sqlx::query(...)`를 써도 된다.
- 예약 상태는 `db::bookings::transition`(내부에서 `BookingStatus::apply` 호출)으로만 바꾼다. `bookings.status`를 직접 UPDATE하지 않는다 (테스트 준비 코드 제외).
- 관리자 API 핸들러는 필요한 권한을 매개변수 타입으로 밝힌다: 조회는 `AdminSession`, 조작은 `RequireOperator`, 정책 변경은 `RequireAdmin` (`docs/decisions.md` D-36). 상태를 바꾸는 관리자 조작은 같은 트랜잭션에서 `record_audit`로 감사 로그를 남기고, 상태 이력의 actor는 `session.actor()`(`admin:<이름>`)를 쓴다.
- HTTP 핸들러에는 `#[utoipa::path(...)]`(경로 · 매개변수 · 응답 · `operation_id`)를 붙이고 `routes!(핸들러)`로 등록한다. 경로 문자열은 이 속성 한 곳에만 둔다 (`docs/decisions.md` D-25). 쿼리 매개변수 구조체는 `#[derive(utoipa::IntoParams)]`처럼 전체 경로로 derive한다 (`use utoipa::IntoParams`로 가져오면 매크로가 만든 코드가 `unused_qualifications`에 걸린다).
- API의 요청 · 응답 모양을 바꾸면 `cd web/admin && pnpm gen`으로 `web/admin/openapi.json`과 `src/api/schema.d.ts`를 다시 만들어 함께 커밋한다 (`scripts/check.sh api` · `web`이 검사한다).
- 이미 적용된 마이그레이션 파일은 수정하지 않는다. 바꿀 것이 있으면 새 마이그레이션을 추가한다.
- 통합 테스트(`tests/`)의 헬퍼 함수에서는 `unwrap()` 대신 `expect("이유")`를 쓴다. clippy의 테스트 예외는 `#[test]` 함수 안에만 적용되기 때문이다.
- 코드를 바꾼 뒤에는 `cargo fmt`와 `cargo clippy`를 통과시킨다.

### 관리자 웹 (`web/admin`)
- 패키지 매니저는 pnpm (버전은 `package.json`의 `packageManager`).
- 서버 데이터는 TanStack Query 훅(`src/api/queries/`)으로만 가져온다 (`useEffect` 안에서 fetch하지 않는다). 바꾸는 요청은 `useMutation`으로 하고, 성공하면 관련 쿼리를 `invalidateQueries`로 다시 읽게 한다.
- `src/api/schema.d.ts`와 `openapi.json`은 자동 생성 파일이므로 직접 고치지 않는다. 백엔드 API가 바뀌면 `pnpm gen`으로 다시 생성한다.
- 의존성은 `package.json`에 정확한 버전(`^` 없이)으로 적고, 공개된 지 1주 이상 지난 버전을 고른다. `pnpm-lock.yaml`도 함께 커밋한다. TypeScript는 5.x를 쓴다 (D-29).
- 코드를 바꾼 뒤에는 `pnpm typecheck`와 `pnpm lint`를 통과시킨다 (`scripts/check.sh web`이 빌드까지 확인한다).

## 품질 검사와 개발 루프

검사는 `scripts/check.sh` 한 곳에 정의되어 있고, 로컬 · Claude Code 훅 · CI가 모두 이 스크립트를 쓴다. 검사 내용을 바꿀 때는 이 스크립트만 고친다.

```bash
scripts/check.sh          # 전체: fmt → clippy → test → sqlx → deny → shear → api → web (CI와 동일)
scripts/check.sh fast     # fmt + clippy (Stop 훅이 쓰는 빠른 검사, DB 불필요)
scripts/check.sh test     # 단계별 실행: fmt | clippy | test | sqlx | deny | shear | api | web
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
| api | `booking-server openapi` | 커밋된 `web/admin/openapi.json`이 서버 코드로 만든 문서와 같은지 (DB 불필요) |
| web | pnpm (openapi-typescript · tsc · ESLint · Vite) | `src/api/schema.d.ts`가 `openapi.json`과 같은지, 타입 검사, ESLint, 빌드 |

Claude Code 훅 (`.claude/settings.json`, `.claude/hooks/`):
- **PostToolUse** (Edit · Write · MultiEdit): 수정한 `.rs` 파일을 즉시 rustfmt로 정리한다. 그래서 편집 직후 파일 내용이 바뀔 수 있다. 이어서 같은 부분을 고칠 때는 파일을 다시 읽는다. `mod x;`를 선언한 파일을 하위 파일보다 먼저 쓰면 그 시점의 rustfmt는 실패하고 조용히 넘어간다. 그런 포맷 누락은 Stop 훅의 fmt 검사가 잡으므로 `cargo fmt --all`로 정리한다.
- **Stop**: 응답을 끝내기 전에 `scripts/check.sh fast`를 돌린다. 실패하면 오류가 돌아오므로 **고친 뒤에** 마무리한다. 두 번째에도 실패하면 막지 않고 사용자에게 경고만 표시된다.
- Stop 훅은 빠른 검사만 한다. 기능을 끝냈다고 보고하기 전에는 `scripts/check.sh`(전체)를 직접 돌려 통과를 확인한다.

규칙:
- 린트를 끄는 `#[allow(...)]`는 꼭 필요할 때만, 가장 좁은 범위(함수 · 문장)에 붙이고 이유를 주석으로 남긴다. `[workspace.lints]`를 느슨하게 바꾸는 것은 사용자와 상의한다.
- 의존성을 추가하면 `Cargo.lock`도 함께 커밋한다 (모든 검사가 `--locked`로 돈다).
- 컴파일러 버전은 `rust-toolchain.toml`에 고정되어 있다. 올릴 때는 버전을 바꾸고 `scripts/check.sh` 통과를 확인한 뒤 별도 커밋으로 올린다.
- 필요한 도구: `brew install cargo-nextest cargo-deny cargo-shear actionlint sqlx-cli` (sqlx-cli는 sqlx 크레이트와 같은 0.9.0), Node.js · pnpm (mise)

## 그 밖의 명령어

```bash
cp .env.example .env                       # 처음 한 번. DATABASE_URL 등 (.env는 커밋하지 않음)
docker compose up -d --wait                # postgres(:5432), otel-lgtm(Grafana :3000, OTLP :4317/4318)
docker compose down                        # 정지 (-v를 붙이면 DB 데이터까지 삭제)
cargo run -p mock-provider                 # mock 업체 서버 (:9090). 장애 주입 로그(debug)는 기본 설정에서 보인다
cargo run -p booking-server -- serve       # 예약 서버 (:8080, API + 워커). 시작할 때 마이그레이션 적용
OTEL_SDK_DISABLED=true cargo run -p booking-server -- serve   # 추적 · 메트릭 · 로그를 내보내지 않고 실행 (콘솔 로그만)
cargo run -p booking-server -- create-client --name <이름>   # API 키 발급 (stdout에 한 번만 출력)
cargo run -p booking-server -- create-admin --username <이름> --role <viewer|operator|admin>   # 관리자 웹 계정 (비밀번호를 한 번만 출력)
cargo run -p booking-server -- openapi     # API 문서(OpenAPI JSON)를 표준 출력으로 (DB 불필요)
(cd web/admin && pnpm install)             # 관리자 웹 의존성 (처음 한 번)
(cd web/admin && pnpm dev)                 # 관리자 웹 개발 서버 http://localhost:5173/admin/ (API는 :8080으로 프록시)
(cd web/admin && pnpm build)               # web/admin/dist → 서버가 http://127.0.0.1:8080/admin/ 에서 제공
(cd web/admin && pnpm gen)                 # API를 바꾼 뒤: openapi.json + src/api/schema.d.ts 다시 만들기
sqlx migrate add <설명>                     # 새 마이그레이션 파일 (migrations/)
cargo sqlx prepare --workspace -- --all-targets   # 쿼리를 바꾼 뒤 .sqlx/ 갱신 (DB 필요)
actionlint                                 # .github/workflows 검사

# tokio-console 개발 모드 (docs/05-observability-and-analytics.md §9). 설치: brew install tokio-console
RUSTFLAGS="--cfg tokio_unstable" CARGO_TARGET_DIR=target/console \
  cargo run -p booking-server --features tokio-console -- serve     # 다른 터미널에서 `tokio-console`
docker compose up -d --force-recreate otel-lgtm   # 알림 규칙(observability/…/alerting)을 고친 뒤 다시 읽히기
```

- 셸 환경에 따라 `cargo`가 PATH에 없을 수 있다. 그럴 때는 `. "$HOME/.cargo/env"`를 먼저 실행한다.
- `cargo nextest`를 직접 돌릴 때는 `DATABASE_URL`이 환경 변수로 있어야 한다 (`set -a; . ./.env; set +a`). `scripts/check.sh`는 기본값을 알아서 넣는다.
- mock 업체는 alpha(멱등키 지원, 매장 `store-001~003`)와 beta(멱등키 없음 · 200으로 오는 에러 · 한국 시간 문자열, 매장 `GN01` · `HD02` · `PG03`) 두 곳이다. 장애 설정은 관리자 웹의 "장애 주입" 화면이나 `curl -X POST localhost:9090/_chaos/<alpha|beta>/preset/<healthy|flaky|slow|throttled|lossy|outage>`로 바꾼다. 관리자 웹으로 하려면 `.env`에 `BOOKING_CHAOS_URL=http://127.0.0.1:9090`이 있어야 한다.
- 관리자 웹(`/admin`)과 관리자 API는 로그인이 필요하다. `create-admin`으로 계정을 만든다. 로컬 http에서 로그인이 유지되지 않으면 `.env`에 `BOOKING_ADMIN_COOKIE_SECURE=false`.
- API 문서는 http://127.0.0.1:8080/docs/ (Swagger UI), 원본은 `/openapi.json`.
- 두 서버는 추적 · 메트릭 · 로그를 OTLP로 `http://localhost:4318`(docker compose의 otel-lgtm)에 내보낸다. Grafana(http://localhost:3000)의 Dashboards → Booking 폴더에서 본다. 수집기가 꺼져 있어도 서버는 돌고, 콘솔에 내보내기 실패 경고만 남는다.
- 대시보드 JSON(`observability/grafana/dashboards/`)은 고치면 10초 안에 Grafana가 다시 읽는다. 고치는 방법은 `observability/README.md`.

## 로컬 포트

| 서비스 | 포트 |
|---|---|
| booking-server | 8080 |
| mock-provider | 9090 |
| PostgreSQL | 5432 |
| Grafana | 3000 |
| OTLP | 4317 (gRPC), 4318 (HTTP, 서버가 쓰는 쪽) |
| tokio-console (개발 모드에서만) | 6669 |
| Vite 개발 서버 | 5173 |
