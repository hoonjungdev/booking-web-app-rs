# 01. 아키텍처 개요

## 1. 목표와 비목표

### 목표
- 여러 외부 예약 업체(provider) API를 하나의 일관된 예약 API로 묶는다.
- 외부 업체가 느리거나, 실패하거나, 호출을 제한해도 **예약 데이터가 틀어지지 않는다** (이중 예약 0건, 유실 0건).
- 외부 호출을 한 건씩 기록하고 측정해서 p95 지연, 성공률, rate limit 사용량을 분석할 수 있다.
- 관리자가 화면에서 상태를 보고, 실패 건을 재처리하고, 업체 정책을 조정할 수 있다.
- 학습 프로젝트이므로 단계마다 실제로 돌려 볼 수 있는 결과물이 나온다.

### 비목표 (지금은 하지 않음)
- 실제 업체 연동, 결제
- 여러 인스턴스로 수평 확장 (서킷 브레이커와 rate limit 상태를 프로세스 메모리에 둔다 → [D-10](decisions.md#d-10-서킷rate-limit-상태는-프로세스-메모리에-단일-인스턴스-전제))
- Kubernetes 배포

## 2. 시스템 구성도

```mermaid
flowchart LR
  client["예약 클라이언트<br/>(다른 서비스 · 앱)"]
  admin["관리자 웹<br/>(React)"]

  subgraph server["booking-server (Rust 단일 바이너리)"]
    api["API 계층<br/>axum"]
    worker["워커<br/>작업 큐 처리"]
    prov["Provider 계층<br/>어댑터 + 장애 대응"]
  end

  db[("PostgreSQL")]
  mock["mock-provider<br/>alpha · beta<br/>(장애 주입)"]
  otel["Grafana LGTM<br/>(OTel 수집 · 저장 · 시각화)"]

  client -- "예약 API /v1" --> api
  admin -- "관리자 API /admin/api" --> api
  api --> db
  worker --> db
  api -- "빈 시간 조회 (동기)" --> prov
  worker -- "예약 · 취소 (비동기)" --> prov
  prov -- HTTP --> mock
  server -. OTLP .-> otel
  mock -. OTLP .-> otel
```

## 3. 구성 요소

| 구성 요소 | 책임 |
|---|---|
| API 계층 | 요청 검증, 인증, 멱등성 처리, 예약 저장과 작업 등록(한 트랜잭션), 조회 |
| 워커 | `jobs` 테이블에서 할 일을 꺼내 업체를 호출하고, 결과에 따라 예약 상태를 바꾸고, 실패하면 다시 예약 |
| Provider 계층 | 업체별 API 차이를 숨기는 **어댑터**, 그 바깥을 감싸는 **장애 대응 파이프라인**(타임아웃 · 재시도 · rate limit · 서킷 브레이커 · bulkhead), 호출 기록 |
| mock-provider | 성격이 다른 가짜 업체 2개(alpha, beta)와 장애 주입 API |
| 관리자 웹 | 대시보드, 예약 · 작업 조회와 재처리, 업체 정책 편집, 장애 주입 제어 |
| PostgreSQL | 예약, 상태 이력, 작업 큐, 업체 호출 기록 |
| Grafana LGTM | 추적(Tempo), 메트릭(Prometheus), 로그(Loki), 대시보드(Grafana) |

## 4. 핵심 흐름: 예약 생성

예약 생성은 **비동기**로 처리한다. 클라이언트에게는 바로 `202 Accepted`를 돌려주고, 실제 업체 호출은 워커가 한다. 이유는 [D-03](decisions.md#d-03-예약-생성은-비동기202--postgresql-작업-큐)에 있다.

```mermaid
sequenceDiagram
  autonumber
  participant C as 클라이언트
  participant A as API
  participant DB as PostgreSQL
  participant W as 워커
  participant P as Provider 계층
  participant M as 외부 업체(mock)

  C->>A: POST /v1/bookings (Idempotency-Key)
  A->>DB: BEGIN → bookings(pending), booking_events, jobs(submit_booking) 저장 → COMMIT
  A-->>C: 202 Accepted {id, status: pending}
  W->>DB: 실행할 작업 가져오기 (FOR UPDATE SKIP LOCKED)
  W->>P: create_booking(booking)
  P->>M: POST 예약 (우리 booking id를 멱등키로)
  M-->>P: 201 확정 / 409 거절 / 5xx / 타임아웃
  P-->>W: Ok(확정) 또는 Err(분류된 에러)
  W->>DB: 상태 전이 (confirmed / rejected / unknown / 재예약)
  C->>A: GET /v1/bookings/{id} (폴링)
  A-->>C: {status: confirmed, provider_booking_id}
```

실패했을 때의 동작(재시도, 결과 불명, 서킷 열림 등)은 [03-resilience.md](03-resilience.md)에서 다룬다.

## 5. 프로세스와 포트

`booking-server`는 바이너리 하나로 API와 워커를 함께 돌린다. 실행 옵션으로 역할을 고를 수 있다(`--role all|api|worker`, 기본값 `all`). → [D-04](decisions.md#d-04-단일-바이너리-역할-선택---role-allapiworker)

| 프로세스 | 포트 | 비고 |
|---|---|---|
| booking-server | 8080 | 예약 API, 관리자 API, 관리자 웹 정적 파일, `/docs`(API 문서) |
| mock-provider | 9090 | `/alpha/...`, `/beta/...`, `/_chaos`, `/_inspect` |
| PostgreSQL | 5432 | Docker |
| Grafana | 3000 | Docker (`grafana/otel-lgtm`) |
| OTLP 수신 | 4317 (gRPC), 4318 (HTTP) | 위와 같은 컨테이너 |
| Vite 개발 서버 | 5173 | 관리자 웹 개발 중에만 |

## 6. 저장소 구조 (Cargo workspace)

```
booking-web-app-rs/
├─ Cargo.toml                 # workspace 정의 (≈ .sln)
├─ crates/
│  ├─ domain/                 # 예약 모델, 상태 머신, 도메인 에러. I/O 없음
│  ├─ telemetry/              # tracing · OpenTelemetry 초기화 (서버와 mock이 함께 씀)
│  ├─ providers/              # ProviderAdapter trait, alpha · beta 어댑터, 장애 대응 파이프라인, 호출 기록기
│  ├─ db/                     # sqlx 기반 저장소(repository) 함수
│  ├─ booking-server/         # 실행 파일: axum API + 워커
│  └─ mock-provider/          # 실행 파일: 가짜 외부 업체 서버
├─ migrations/                # sqlx 마이그레이션 SQL
├─ web/admin/                 # React 관리자 웹 (Rust 크레이트 아님)
├─ observability/             # Grafana 대시보드 JSON, 알림 규칙
├─ docker-compose.yml         # postgres, otel-lgtm
└─ docs/
```

크레이트 의존 방향(화살표 = "사용한다"). `domain`은 다른 크레이트에 의존하지 않으므로 가장 먼저 만들고 가장 쉽게 테스트할 수 있다.

```mermaid
flowchart BT
  domain
  telemetry
  providers --> domain
  db --> domain
  server["booking-server"] --> providers
  server --> db
  server --> telemetry
  mock["mock-provider"] --> telemetry
```

mock-provider는 일부러 `domain`에 의존하지 않는다. 진짜 외부 업체처럼 우리 코드와 독립적이어야 하기 때문이다.

## 7. 기술 스택

| 영역 | 선택 | .NET에서 비슷한 것 |
|---|---|---|
| 언어 / 빌드 | Rust (stable), Cargo | C#, dotnet CLI + NuGet |
| 비동기 런타임 | tokio | Task + ThreadPool |
| 웹 프레임워크 | axum | ASP.NET Core Minimal API |
| 미들웨어 | tower, tower-http | ASP.NET Core 미들웨어 |
| 장애 대응 | tower + governor(rate limit) + 직접 구현(서킷 브레이커, 재시도) | Polly |
| JSON | serde, serde_json | System.Text.Json |
| DB | PostgreSQL + sqlx | Dapper (단, SQL을 컴파일 시점에 검사) |
| 마이그레이션 | sqlx-cli | EF Core Migrations / DbUp |
| HTTP 클라이언트 | reqwest | HttpClient |
| 설정 | figment (파일 + 환경 변수) | IConfiguration + IOptions |
| 런타임 설정 교체 | arc-swap | IOptionsMonitor |
| 캐시 | moka | IMemoryCache |
| 로그 · 추적 | tracing + opentelemetry | ILogger + Activity (System.Diagnostics) |
| API 문서 | utoipa + Swagger UI | Swashbuckle / NSwag |
| 에러 타입 | thiserror(라이브러리), anyhow(실행 파일) | 커스텀 Exception 계층 |
| ID | UUID v7 (`uuid` 크레이트) | `Guid.CreateVersion7()` (.NET 9+) |
| 시간 | chrono | DateTimeOffset |
| 테스트 | cargo test / cargo-nextest, wiremock, testcontainers, insta | xUnit, WireMock.Net, Testcontainers, Verify |
| 부하 테스트 | oha, goose | NBomber, k6 |
| 관리자 웹 | React + TypeScript + Vite, Mantine, TanStack Query | (Blazor 대신) |

## 8. C# 개발자를 위한 Rust 개념 대응표

| C# | Rust | 메모 |
|---|---|---|
| `interface` | `trait` | 이미 있는 타입에도 나중에 구현을 붙일 수 있다 |
| 예외 (`throw` / `catch`) | `Result<T, E>` + `?` 연산자 | 실패 가능성이 함수 시그니처에 드러난다 |
| `null`, `T?` | `Option<T>` | null이 없다. 컴파일러가 `None` 처리를 강제한다 |
| 판별 공용체 흉내 (abstract record 계층) | 값을 담는 `enum` + `match` | 예약 상태와 에러 분류에 적극 사용한다 |
| DI 컨테이너 | 없음. `AppState`를 만들어 axum `State`로 전달 | 의존성이 코드에 명시적으로 드러난다 |
| `BackgroundService` | `tokio::spawn`으로 띄운 태스크 | 워커, 호출 기록기 |
| `CancellationToken` | `tokio_util::sync::CancellationToken` | 정상 종료(graceful shutdown) |
| `SemaphoreSlim` | `tokio::sync::Semaphore` | bulkhead |
| `Channel<T>` | `tokio::sync::mpsc` | 호출 기록기 |
| `Func<Task<T>>`를 받는 제네릭 메서드 | 제네릭 함수 + 클로저 (`F: FnMut() -> Fut`) | `ResilientProvider::call`: 재시도마다 클로저를 다시 불러 새 요청을 만든다 |
| `task.WaitAsync(timeout)` | `tokio::time::timeout` | 시간 안에 끝나지 않으면 Future를 버린다 (요청 취소) |
| `using` / `IDisposable` | `Drop` (값이 사라질 때 자동 정리) | bulkhead 자리(`SemaphorePermit`)는 drop될 때 반납된다 |
| `Interlocked.Increment` | `AtomicU64::fetch_add` | 잠금 없이 여러 스레드가 함께 올리는 카운터 |
| `System.Threading.RateLimiting` | governor | 업체별 rate limiter |
| GC | 소유권(ownership)과 빌림(borrowing) | 여러 곳에서 공유하려면 `Arc<T>` (참조 카운트 포인터) |
| `.csproj` / `.sln` | crate / workspace | |
| `[Attribute]` | `#[derive(...)]`, 속성 매크로 | `#[derive(Serialize)]`는 JSON 직렬화 코드를 자동 생성 |
