# 설계 결정 기록 (ADR)

결정을 바꿀 때는 기존 항목을 지우지 않는다. 상태를 `대체됨 (→ D-xx)`으로 바꾸고 새 항목을 추가한다.

### D-01. 백엔드: Rust + tokio + axum
- 상태: 채택
- 이유: tokio 팀이 관리하는 사실상 표준 조합이다. tower 미들웨어 생태계를 서버(들어오는 요청)와 업체 호출(나가는 요청) 양쪽에 같은 방식으로 쓸 수 있다.
- 대안: actix-web (독자 생태계), Loco (추상화가 많아 학습용으로 맞지 않음)

### D-02. DB: PostgreSQL + sqlx, ORM 없음
- 상태: 채택
- 이유: SQL을 직접 쓰되 컴파일 시점에 실제 스키마로 검사한다. `FOR UPDATE SKIP LOCKED`, `percentile_cont` 같은 PostgreSQL 기능을 그대로 쓸 수 있다.
- 결과: 빌드할 때 DB 연결이나 오프라인 메타데이터(`.sqlx/`, 커밋함)가 필요하다.
- 대안: SeaORM, Diesel

### D-03. 예약 생성은 비동기(202) + PostgreSQL 작업 큐
- 상태: 채택
- 이유: 업체가 느리거나 죽어 있어도 접수는 바로 성공한다. 예약 저장과 작업 등록이 한 트랜잭션이라 유실이 없다(Transactional Outbox). 메시지 브로커 없이 인프라를 단순하게 유지한다.
- 결과: 클라이언트는 폴링으로 결과를 확인한다. 처리량이 아주 커지면 브로커 도입을 다시 검토한다.
- 대안: 동기 호출 (업체 장애가 곧 우리 장애가 됨), RabbitMQ / Kafka (학습 범위에 비해 과함)

### D-04. 단일 바이너리, 역할 선택 (`--role all|api|worker`)
- 상태: 채택
- 이유: 로컬 실행과 배포가 단순하다. 나중에 API와 워커를 따로 띄울 수 있는 길은 열어 둔다.

### D-05. 장애 대응은 작은 부품을 조합하고, 서킷 브레이커와 재시도는 직접 구현
- 상태: 채택
- 이유: 동작을 완전히 이해하고 원하는 곳에 관측 지점을 넣기 위해서다. 학습 가치도 크다. rate limit만은 검증된 governor를 쓴다.
- 대안: failsafe 크레이트, tower의 retry 계층 그대로 사용

### D-06. 관측은 OpenTelemetry(OTLP) 단일 경로, 로컬은 grafana/otel-lgtm
- 상태: 채택
- 이유: 특정 업체에 묶이지 않는다. Datadog이나 OpenObserve 등으로 바꿔도 코드를 고치지 않는다. Docker 컨테이너 하나로 추적 · 메트릭 · 로그 · 대시보드가 모두 뜬다.

### D-07. 업체 호출 통계는 메트릭과 DB 호출 기록 두 갈래로
- 상태: 채택
- 이유: 메트릭은 실시간이고 저렴하지만 p95가 근사값이고 조건 분석이 제한적이다. 호출 기록은 정확하고 예약 단위로 추적할 수 있다.
- 결과: `provider_calls`의 보관 기간(30일)을 관리해야 한다.

### D-08. 관리자 웹: React + TypeScript + Vite + Mantine + TanStack Query, 타입은 OpenAPI에서 생성
- 상태: 채택
- 이유: React 기초를 이미 알고 있고, 백엔드 학습에 집중하기 위해 프론트는 생산성 높은 조합을 쓴다. 타입 자동 생성으로 백엔드와 프론트의 계약 불일치를 컴파일 단계에서 잡는다.
- 대안: Leptos (Rust 풀스택, 학습 부담 큼), htmx + 서버 템플릿

### D-09. mock 업체를 Rust로 직접 구현, 성격이 다른 두 곳(alpha, beta)
- 상태: 채택
- 이유: 장애를 마음대로 주입하기 위해서다. 두 업체의 API 스타일 차이 때문에 어댑터 패턴이 실제로 필요해진다.

### D-10. 서킷·rate limit 상태는 프로세스 메모리에 (단일 인스턴스 전제)
- 상태: 채택
- 이유: 단순하다.
- 결과: 인스턴스를 여러 개 띄우면 업체 한도를 인스턴스 수만큼 넘길 수 있다. 확장할 때 Redis로 옮긴다.

### D-11. 식별자는 UUID v7, 시각은 UTC `timestamptz`
- 상태: 채택
- 이유: UUID v7은 시간순으로 정렬되어 B-tree 인덱스에 유리하고, 업체에 보내는 참조값으로도 그대로 쓸 수 있다.

### D-12. 에러 응답은 RFC 9457 Problem Details
- 상태: 채택
- 이유: 표준 형식이다. 클라이언트가 `type`으로 에러 종류를 구분할 수 있다.

### D-13. 품질 게이트는 단일 스크립트 + 고정 toolchain, 세 곳(로컬 · Claude Code 훅 · CI)에서 같은 명령 실행
- 상태: 채택
- 이유: 로컬에서 통과한 코드가 CI에서 깨지는 일을 없앤다. Claude Code가 응답을 끝내기 전에 Stop 훅이 fmt + clippy를 돌려, 실패하면 Claude가 스스로 고치게 한다. 코딩 규칙(`unwrap` · `println!` 금지 등)은 문서로만 두지 않고 clippy 린트로 강제한다.
- 구성: `scripts/check.sh`(fmt · clippy · nextest · cargo-deny · cargo-shear), `rust-toolchain.toml`(1.99.1 고정), `[workspace.lints]`, `.claude/settings.json` 훅, `.github/workflows/ci.yml`, Dependabot
- 결과: toolchain과 CI 도구 버전은 사람이 직접 올린다. Stop 훅 때문에 응답이 끝날 때마다 수 초가 더 걸린다 (Rust 변경이 없으면 건너뜀).
- 대안: pre-commit 훅 (커밋할 때만 돌아서 Claude의 작업 중간 피드백이 늦음), `just`/`make` (도구 추가 설치 필요)

### D-14. API 키는 256비트 무작위 값, DB에는 SHA-256 해시만 저장
- 상태: 채택
- 이유: 비밀번호는 사람이 정해 짧고 추측하기 쉬워서 느린 해시(argon2 등)가 필요하다. API 키는 서버가 만든 256비트 무작위 값이라, 빠른 해시로도 역산하거나 추측할 수 없다. 빠른 해시라서 요청마다 인증해도 비용이 거의 없고, 해시 값으로 바로 DB를 조회할 수 있다.
- 결과: 키 원문은 발급할 때 한 번만 보여 준다(`booking-server create-client`). 잃어버리면 새로 발급한다.
- 대안: argon2 (요청마다 수십 ms가 들고, 해시로 바로 조회할 수 없어 키에 별도 식별자가 필요함)

### D-15. sqlx 쿼리 검사는 커밋된 오프라인 데이터(`.sqlx/`)로, 최신 여부는 별도 단계로 확인
- 상태: 채택
- 이유: CI와 Stop 훅에서 DB 없이 컴파일할 수 있다. SQL을 바꾸고 `cargo sqlx prepare`를 잊으면 바로 컴파일 에러가 난다.
- 구성: `scripts/check.sh`가 `SQLX_OFFLINE=true`를 기본으로 쓴다. `sqlx` 단계는 마이그레이션을 적용한 뒤 `cargo sqlx prepare --check`로 `.sqlx/`가 스키마와 맞는지 확인한다. CI의 `test` · `sqlx` job에는 PostgreSQL 서비스 컨테이너를 붙인다.
- 결과: 쿼리를 바꾸면 `cargo sqlx prepare --workspace -- --all-targets`를 돌려 `.sqlx/`를 함께 커밋해야 한다. sqlx CLI 버전은 sqlx 크레이트(0.9.0)와 맞춘다.

### D-16. 업체 어댑터는 `async-trait`으로 만든 `dyn` trait
- 상태: 채택
- 이유: 업체마다 다른 어댑터를 실행 중에 업체 ID로 골라 써야 해서 `Arc<dyn ProviderAdapter>`가 필요하다. Rust 1.99에서도 `async fn`이 있는 trait은 `dyn`으로 쓸 수 없어서(E0038) `async-trait` 크레이트로 해결한다. 호출마다 Future를 힙에 하나 할당하지만, 네트워크 호출에 비하면 무시할 만하다.
- 대안: enum으로 어댑터를 나열해 `match`로 고르기 (업체를 추가할 때마다 enum과 모든 메서드를 고쳐야 하고, 테스트용 가짜 어댑터를 넣기 어려움)

### D-17. 어댑터는 결과와 함께 응답 메타를 돌려준다
- 상태: 채택
- 이유: `provider_calls`에 HTTP 상태와 `X-RateLimit-Remaining`을 남겨야 하는데, 결과 타입(`Result<T, ProviderError>`)만으로는 성공 응답의 상태 코드나 헤더를 알 수 없다. 그래서 어댑터 메서드는 `AdapterReply<T> { result, meta }`를 돌려준다. 어댑터 안에서는 `?`를 쓰는 내부 함수가 `&mut ResponseMeta`를 채우고, trait 메서드가 둘을 묶는다. `ResilientProvider`가 메타는 기록에 쓰고 결과만 바깥에 넘기므로 워커는 이 구조를 모른다.
- 대안: `ProviderError`에 상태 코드를 넣기 (성공 응답의 정보는 담을 수 없음), 어댑터가 마지막으로 본 값을 공유 변수에 두기 (동시 호출끼리 값이 섞임)

### D-18. 호출 기록은 providers가 채널로 보내고, DB 저장은 booking-server가 맡는다
- 상태: 채택
- 이유: providers가 db 크레이트에 의존하지 않게 해서 크레이트 의존 방향을 지킨다 ([01 §6](01-architecture.md#6-저장소-구조-cargo-workspace)). 보내는 쪽은 `try_send`만 하므로 업체 호출 경로가 DB를 기다리지 않는다.
- 결과: 기록 태스크는 보내는 쪽(`CallRecorder`)이 모두 사라질 때 끝난다. 서버가 멈출 때 워커가 먼저 끝나야 남은 기록이 저장된다. 기록이 필요 없는 테스트는 `CallRecorder::discard()`를 쓴다.
- 대안: providers → db 의존 (방향 위반), 기록용 trait 객체 (구현이 하나뿐이라 지금은 채널로 충분)

### D-19. 워커의 업체별 동시 처리 제한은 두 번째 업체(Phase 6)와 함께 만든다
- 상태: 채택 ([03 §8](03-resilience.md#8-bulkhead-격리)의 일부를 뒤로 미룸)
- 이유: 지금은 업체가 alpha 하나뿐이라 업체 사이의 격리를 만들어도 시험할 수 없다. 업체 쪽 bulkhead(Semaphore)는 Phase 3에서 구현해서, 한 업체에 대한 동시 호출 수는 이미 제한된다.
- 결과: Phase 6 전까지는 한 업체가 느려지면 워커 루프(기본 4개)가 모두 그 업체 호출에 묶일 수 있다 (호출 예산 15초가 상한). 서킷이 열리면 호출이 바로 끝나므로 풀린다. "한 업체만 장애" 시나리오 테스트도 Phase 6에서 추가한다.

### D-20. 라이브러리 크레이트는 OpenTelemetry API만 쓰고, SDK 구성은 telemetry 크레이트에만 둔다
- 상태: 채택
- 이유: OpenTelemetry는 **계측 API**(`opentelemetry` 크레이트)와 **수집 · 전송 구현**(`opentelemetry_sdk`, `opentelemetry-otlp`)을 나눠 놓았다. 라이브러리는 API로 값을 기록만 하고, 어디로 내보낼지는 실행 파일이 정한다. .NET에서 라이브러리가 `System.Diagnostics.Metrics.Meter` · `ActivitySource`만 쓰고 OpenTelemetry SDK는 앱이 구성하는 것과 같다. 이렇게 하면 `providers`가 `telemetry`(axum · SDK · exporter를 끌고 온다)에 의존하지 않아도 되고, SDK를 설치하지 않은 테스트에서는 계측 코드가 아무 일도 하지 않는다.
- 구성
  - `providers`: `opentelemetry`(API) + `opentelemetry-http`(헤더에 `traceparent` 넣기). 시도마다 쌓이는 지표(시간, 재시도, 호출 제한, 버린 기록)를 직접 기록하고, 업체 요청에 trace 정보를 싣는다.
  - `booking-server`: `opentelemetry`(API)로 예약 · 작업 큐 지표를 기록한다. 서킷 · pause · 동시 실행 수 같은 **현재 상태**는 `ResilientProvider`가 값을 내주고(`circuit_state()`, `paused_until()`, `in_flight()`, `rate_limit_remaining()`), 서버가 지표를 내보낼 때마다 읽어 간다. 같은 값을 Phase 5의 관리자 API(업체 런타임 상태)도 쓴다.
  - `db`, `domain`: OpenTelemetry를 모른다. trace ID는 문자열과 JSON으로 받아 저장만 한다 (`db::TraceRef`).
  - `telemetry`: SDK · exporter · tracing layer 조립, HTTP 서버 미들웨어, trace 전파 도우미.
- 결과: `providers`는 "지금 OpenTelemetry Context"에서 trace 정보를 읽는다. tracing span에 들어가면 그 span이 지금 Context가 되는 것은 `tracing-opentelemetry`의 기본 동작(context activation)이다. 통합 테스트(`tests/telemetry.rs`)가 업체 쪽 span이 같은 trace로 이어지는지 검사하므로, 이 동작이 꺼지면 테스트가 실패한다.
- 대안: 지표를 trait(관찰자)으로 뒤집어 서버가 구현 (호출 기록 채널처럼. 지표마다 메서드가 늘고 얻는 것이 없다), `providers` → `telemetry` 의존 (의존 방향이 늘고 무거워진다), tracing 이벤트로 지표 만들기(`MetricsLayer`. 버킷 지정과 관측형 gauge를 쓸 수 없다)

### D-21. OTLP는 HTTP/protobuf로 보내고, 내보내기는 SDK의 전용 스레드에 맡긴다
- 상태: 채택
- 이유: `opentelemetry-otlp`의 기본 구성이다. gRPC(tonic)를 쓰지 않아 의존성이 가볍고, 내보내기가 tokio 런타임과 분리된다. SDK가 신호마다 전용 스레드를 띄워 그 안에서 blocking HTTP 클라이언트로 보내므로, 요청을 처리하는 태스크는 메모리 큐에 넣기만 한다 ("관측이 서비스를 느리게 하면 안 된다").
- 결과: 수집기 주소의 기본값은 `http://localhost:4318`이다 (gRPC 포트 4317이 아니다). 메트릭은 10초마다 내보낸다 (SDK 기본값 60초는 로컬에서 대시보드를 보며 실험하기에 길다. `OTEL_METRIC_EXPORT_INTERVAL`로 바꾼다).
- 대안: gRPC (tonic과 tokio 런타임이 필요하고, 얻는 것은 스트리밍 정도인데 지금은 쓸 일이 없다)

### D-22. 로그 필터는 전역으로 걸고, per-layer filter는 tokio-console 모드에서만 쓴다
- 상태: 채택
- 배경: tracing-subscriber에서 필터를 거는 방법은 둘이다. **전역 필터**는 걸린 span · 이벤트를 어느 layer에도 보내지 않는다. **per-layer filter**는 layer마다 따로 거른다 (예: 콘솔은 info, OpenTelemetry는 debug).
- 이유: per-layer filter를 쓰면 span과 로그가 **가끔 사라진다**. per-layer filter는 "이번 이벤트를 어느 layer가 껐는지"를 스레드별 상태에 적어 두는데, 이벤트를 남기지 않고 "이 수준이 켜져 있나?"만 묻는 코드(`tracing::enabled!`, `log::log_enabled!`)가 있으면 그 상태가 지워지지 않고 남아 같은 스레드의 다음 span · 이벤트가 버려진다. sqlx가 쿼리마다 이렇게 묻기 때문에, 처음 구현했을 때 요청 span과 로그가 무작위로 빠졌다 (응답에 `X-Trace-Id`가 붙었다 안 붙었다 했다). 전역 필터에는 이 상태가 없다.
- 결과: 전역 필터 하나로는 "콘솔은 warn, 추적은 info"처럼 나눠 걸 수 없다. 그대로 두면 로그를 줄이려고 `RUST_LOG=warn`으로 올렸을 때 span까지 꺼져 추적과 `X-Trace-Id`가 사라진다. 그래서 전역 필터를 직접 만들었다 (`telemetry::filter::GlobalFilter`): **이벤트(로그)는 `RUST_LOG`를 따르고, span은 info 이상이면 항상 통과**시킨다. 콘솔 로그와 Loki로 가는 로그에는 같은 필터가 적용된다.
- 결과: 내보내는 과정에서 생긴 로그가 다시 내보내지는 되먹임도 per-layer filter 없이 막아야 한다. SDK는 자기 내보내기 스레드에서 생긴 로그만 거르고, exporter의 HTTP 클라이언트(별도 스레드)가 남기는 로그는 거르지 못한다. 로그 브리지를 감싸는 평범한 layer(`WithoutExporterLogs`)가 그 대상(`reqwest` · `hyper` · `h2` · `opentelemetry`)의 로그를 넘기지 않는다.
- 예외: tokio-console은 `RUST_LOG`와 상관없이 런타임 내부 이벤트(`tokio=trace`)를 모두 받아야 해서, 그 모드(`tokio-console` feature)에서만 우리 layer들에 per-layer filter를 건다. 위 문제를 피하려고 필터를 매번 다시 평가하게 한다 (`dynamic_filter_fn`. 남은 상태를 덮어쓴다). 그만큼 느려지므로 개발 모드에 한정한다. 이 모드에서는 span도 `RUST_LOG`를 따른다.
- 대안: 모든 빌드에서 per-layer filter + 우회책 (tracing-subscriber 내부 동작에 기대는 범위가 넓어지고, 모든 이벤트에서 필터를 다시 평가한다)

### D-23. 알림을 계산할 수 있도록 지표를 보완한다 (긴 버킷, `booking.unknown.oldest_age`)
- 상태: 채택 ([05 §4](05-observability-and-analytics.md#4-메트릭-목록)의 버킷과 지표 목록을 보완)
- 이유: 처음 설계의 지표만으로는 [05 §8](05-observability-and-analytics.md#8-대시보드와-알림)의 알림 둘을 계산할 수 없었다.
  - "작업 지연 p95가 30초 초과": 히스토그램 버킷이 10초에서 끝나면 그 위의 값은 모두 한 칸에 들어가서, p95가 10초보다 크다는 것까지만 알 수 있다. → `jobs.lag`와 `booking.time_to_final`에는 30분까지 이어지는 버킷을 쓴다.
  - "`unknown` 상태가 10분 넘게 유지": 상태 전이 횟수(`booking.transitions`)로는 "지금 unknown인 예약이 얼마나 오래됐는지"를 알 수 없다. → 가장 오래된 unknown 예약의 경과 시간을 gauge(`booking.unknown.oldest_age`)로 추가했다. `bookings (status, updated_at)` 인덱스로 한 건만 찾는다.
- 결과: 히스토그램 버킷이 두 벌이다 (짧은 호출용 · 긴 흐름용). `tokio.runtime.*` 지표도 추가했다 (Service 대시보드의 런타임 패널용).
- 같은 이유로 "서킷이 5분 넘게 Open" 알림은 서킷 상태 값만 보지 않고 "서킷에 막힌 호출이 계속 있는가"를 함께 본다. 서킷은 다음 호출이 올 때에야 Open에서 벗어나므로, 상태 값만 보면 업체가 복구된 뒤에도 (처리할 예약이 없는 동안) 계속 울린다.

### D-24. 헬스 체크 요청은 추적과 지표에서 뺀다
- 상태: 채택 ([04 §1](04-api.md#1-공통-규약)의 "모든 응답 헤더에 `X-Trace-Id`"에 예외를 둠)
- 이유: `/healthz` · `/readyz`는 배포 환경에서 몇 초마다 불린다. 계측하면 trace 목록이 헬스 체크로 채워지고, API 요청 수 · 지연 지표가 실제 요청이 아닌 값에 묻힌다.
- 결과: 헬스 체크 응답에는 `X-Trace-Id`가 없고 `http.server.request.duration`에도 잡히지 않는다. 라우터에서 계측 layer를 건 **뒤에** 헬스 체크 라우트를 등록하는 것으로 구현했다 (`api::router`).

### D-25. API 문서는 utoipa로 코드에서 만들고, 라우트와 문서 경로를 한곳에서 정한다
- 상태: 채택 ([D-08](#d-08-관리자-웹-react--typescript--vite--mantine--tanstack-query-타입은-openapi에서-생성)의 "타입은 OpenAPI에서 생성"을 구현)
- 이유: 관리자 웹의 TypeScript 타입을 서버 코드에서 만들려면 OpenAPI 문서가 코드와 항상 같아야 한다. utoipa-axum의 `routes!(핸들러)`는 핸들러의 `#[utoipa::path(path = ..)]`를 읽어 axum 라우트와 문서 경로를 함께 등록하므로, 경로 문자열이 한곳에만 있어 둘이 어긋날 수 없다 (≈ Swashbuckle이 컨트롤러 라우트로 문서를 만드는 것).
- 구성
  - `booking-server openapi`가 문서를 표준 출력으로 낸다 (DB 없이). 결과를 `web/admin/openapi.json`으로 커밋하고, `scripts/check.sh api`가 코드와 같은지 검사한다.
  - `domain`의 예약 상태 · ID · 고객 타입은 `openapi` feature를 켤 때만 `ToSchema`를 derive한다 (`#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]`). 문서를 만드는 booking-server만 켜고, 다른 크레이트에는 utoipa가 컴파일되지 않는다. derive는 컴파일 시점의 메타데이터라 "domain에 I/O를 넣지 않는다"는 규칙과 부딪히지 않는다. 학습용 주석이 문서 설명으로 새지 않게 `schema(description = ..)`을 따로 적는다.
  - 업체 정책은 API 전용 타입(`PolicyView`)으로 내보낸다. 정책 타입은 `#[serde(default)]`라서 그대로 쓰면 모든 필드가 "생략 가능"으로 문서화되기 때문이다. 정책 → 응답 변환은 필드를 `..` 없이 모두 풀어 써서, 정책에 필드가 늘면 컴파일 에러로 드러난다.
  - mock 업체의 장애 설정도 booking-server에 같은 모양의 타입을 따로 둔다 (mock-provider는 우리 크레이트에 의존하지 않는다).
- 결과: 서버 API를 바꾸면 `cd web/admin && pnpm gen`으로 문서와 타입을 다시 만들어 함께 커밋해야 한다 (잊으면 CI의 `api` · `web` 단계가 실패한다).
- 대안: OpenAPI 문서를 손으로 관리 (코드와 어긋난다), utoipa만 쓰고 라우트는 axum에 따로 등록 (경로 문자열이 두 곳), domain이 utoipa에 늘 의존 (문서가 필요 없는 크레이트까지 컴파일)

### D-26. 관리자 재시도는 예약을 잠그고, dead 작업을 같은 행으로 되살리고, 기한을 다시 잡고, 보내기 전에 업체에서 먼저 찾는다
- 상태: 채택 ([03 §5](03-resilience.md#5-결과-불명-unknown-outcome-처리)의 "관리자 재시도는 다시 제출하기 전에 업체 조회를 먼저"를 구현)
- 규칙 (`POST /admin/api/bookings/{id}/retry`, `POST /admin/api/jobs/{id}/requeue` 공통, 한 트랜잭션)
  1. 예약 행을 잠그고(`SELECT … FOR UPDATE`) `failed`인지, 대기 · 실행 중인 작업이 없는지 확인한다. 한 예약에 처리 중인 작업이 둘이면 두 워커가 같은 예약을 동시에 업체에 보낼 수 있다 (멱등키 없는 업체에서는 이중 예약).
  2. `failed → pending` (`AdminRetry`, actor `admin`), 확정 기한을 지금부터 다시 계산한다 (접수와 같은 규칙: 30분 뒤 또는 슬롯 시작 중 이른 쪽). 슬롯이 이미 시작했으면 409.
  3. 그 예약의 dead 작업을 **같은 행으로** 되살린다 (`submit_booking`, 시도 횟수 0, payload `{"lookup_first": true}`). dead 작업이 없으면 새로 등록한다.
  4. 워커는 `lookup_first`가 있으면 업체에서 참조값으로 먼저 찾는다: 있으면 보내지 않고 `confirmed`, 없으면 제출, 조회 실패면 작업 재예약(표시는 남아 다음에도 조회부터).
- 이유
  - 기한 재설정: 장애로 실패한 예약은 대개 업체를 부르지 못한 채 기한이 지나 포기한 것이다. 처음 구현은 기한을 그대로 둬서, 재시도하자마자 워커가 `deadline_exceeded`로 다시 포기했다 (관리자 화면으로 끝에서 끝까지 돌려 보다 발견).
  - 같은 행 되살리기: dead 작업 수(`jobs.dead` 지표 · "dead 작업이 있음" 알림 · 대시보드)가 재처리하는 즉시 줄어든다. 새 작업을 만들면 dead 행이 남아 따로 치울 방법(새 작업 상태, 마이그레이션)이 필요하다. 결과 불명 확인(`resolve_unknown`)이 포기한 예약도 같은 행을 `submit_booking` + `lookup_first`로 바꿔 되살린다: "조회 → 없으면 제출"이 그 예약에 지금 필요한 일이다.
  - "없음"을 한 번만 확인: 결과 불명 확인은 업체 처리가 늦을 수 있어 두 번 확인하지만, 관리자 재시도는 실패하고 한참 뒤(사람이 보고 누른 뒤)라 한 번으로 판단한다.
- 결과
  - 작업 재큐잉은 예약이 `failed`일 때 위 규칙과 같고, 그 밖에는 예약은 그대로 두고 작업만 같은 내용으로 되살린다 (예약이 없어 묻힌 작업 등. 같은 예약에 처리 중인 작업이 따로 있으면 409).
  - 참조값 조회를 지원하지 않는 업체는 확인 없이 다시 보낸다 (관리자가 업체에 직접 확인했다고 본다). 지금은 그런 업체가 없다.
  - 재시도한 예약의 `booking.time_to_final`은 처음 접수한 시각부터 잰다 (재시도 전 시간이 포함된다).
  - `cancel_failed` 재시도는 취소 흐름과 함께 Phase 6에서 만든다 (지금은 409).
- 대안: 새 작업 등록 + dead 작업 그대로 (dead 수가 줄지 않음), 작업 상태에 `retried` 추가 (마이그레이션 · 지표 · 화면을 모두 고쳐야 함), `failed → unknown` 전이를 만들어 결과 불명 확인 재사용 (상태 머신 변경)

### D-27. 장애 주입 중계는 `BOOKING_CHAOS_URL`을 설정할 때만 켠다
- 상태: 채택
- 이유: mock 업체의 `/_chaos`는 개발 도구이고 실제 업체에는 없다. 기본으로 켜 두면 다른 환경에서도 쓸모없는 관리자 API가 열려 있고, 누르면 엉뚱한 주소로 요청이 나간다.
- 결과: 로컬에서는 `.env`에 `BOOKING_CHAOS_URL=http://127.0.0.1:9090`이 있어야 장애 주입 화면을 쓴다 (`.env.example`에 넣어 두었다). 꺼져 있으면 관리자 API가 503(`chaos-disabled`)을 돌려주고, 화면은 켜는 방법을 안내한다.
- 대안: 업체의 `base_url`에서 mock 주소를 추론 (업체 API와 관리 API가 같은 서버라는 가정에 기댄다), 관리자 웹이 mock을 직접 호출 (주소가 둘, CORS 필요)

### D-28. 업체 정책 편집은 정책 실시간 반영(Phase 6)과 함께 만든다
- 상태: 채택 ([04 §3](04-api.md#3-관리자-api-adminapi)의 정책 변경 `PUT /admin/api/providers/{id}/policy`를 Phase 6으로 미룸)
- 이유: 설계는 "정책 변경 (즉시 반영)"이다. 지금 `ResilientProvider`는 서버가 시작할 때 정책을 읽어 서킷 · rate limiter · bulkhead를 만들고 바꾸지 않는다. 즉시 반영하려면 정책과 그 부품을 실행 중에 교체해야 하는데(arc-swap), 로드맵상 Phase 6 항목이다. 저장만 하고 재시작해야 반영되는 편집 화면은 "바꿨는데 그대로인" 혼란을 준다.
- 결과: Phase 5의 업체 상세 화면은 정책을 읽기 전용으로 보여 준다.

### D-29. 관리자 웹 의존성은 정확한 버전으로 고정하고, 공개된 지 1주 이상 지난 버전을 쓴다. TypeScript는 5.x
- 상태: 채택
- 이유
  - 공급망 공격: 공개 직후의 버전에 악성 코드가 섞였다가 며칠 안에 철회되는 일이 있다. 패키지를 처음 고를 때 1~2주 지난 버전을 골랐고, Dependabot의 npm 업데이트도 공개 7일 뒤에 올린다(`cooldown`). pnpm 11도 기본으로 갓 나온 버전을 막는다(`minimumReleaseAge`). `package.json`에 `^` 없이 정확한 버전을 적고 `pnpm-lock.yaml`을 커밋한다.
  - TypeScript 7(Go로 다시 만든 컴파일러)은 openapi-typescript(`typescript ^5`)와 typescript-eslint(`<6.1`)가 아직 지원하지 않는다.
- 결과: 일부 패키지가 최신보다 한두 버전 낮다. 올릴 때는 Dependabot PR이 CI(`web` 단계)를 통과하는지 본다.
