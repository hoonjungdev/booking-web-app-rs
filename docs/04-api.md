# 04. API 설계

## 1. 공통 규약

| 항목 | 규칙 |
|---|---|
| 형식 | JSON, 필드명 `snake_case` |
| 시각 | RFC 3339 (`2026-11-20T19:00:00+09:00`). 응답은 항상 UTC(`Z`). 마이크로초까지만 쓰고 더 작은 단위는 버린다 (PostgreSQL 정밀도) |
| ID | UUID v7 문자열 |
| 에러 | RFC 9457 Problem Details (`application/problem+json`). ASP.NET Core의 `ProblemDetails`와 같은 형식 |
| 페이지네이션 | 커서 방식: `?limit=50&cursor=<불투명 문자열>` → 응답에 `next_cursor` |
| 추적 | 응답 헤더에 `X-Trace-Id`, 에러 본문에도 같은 값의 `trace_id` (헬스 체크 제외). 요청에 W3C `traceparent` 헤더를 실어 보내면 그 trace에 이어 붙는다 |
| 인증 | `X-Api-Key` 헤더. 키는 `booking-server create-client --name <이름>`으로 발급하며, 발급할 때 한 번만 보여 준다 |
| 문서 | `/openapi.json`, `/docs` (Swagger UI). utoipa로 코드에서 생성 ([D-25](decisions.md#d-25-api-문서는-utoipa로-코드에서-만들고-라우트와-문서-경로를-한곳에서-정한다)). 관리자 웹의 TypeScript 타입도 이 문서에서 만든다 ([06 §4](06-admin-frontend.md#4-타입-생성-흐름)) |

에러 예:
```json
{
  "type": "https://booking.local/problems/idempotency-key-reused",
  "title": "같은 Idempotency-Key로 다른 요청이 들어왔습니다",
  "status": 422,
  "detail": "이 키는 다른 요청 본문으로 이미 사용되었습니다.",
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736"
}
```

`X-Trace-Id`는 그 요청의 처리 과정을 Grafana(Tempo)에서 찾는 열쇠다. 문의를 받을 때 이 값을 함께 받으면 어떤 DB 쿼리와 업체 호출이 있었는지 바로 볼 수 있다. 예약 접수의 경우 업체 호출은 워커가 따로 하므로, 접수 요청의 trace에는 접수까지만 있고 업체 호출은 그 예약의 작업 trace에 있다 (예약 ID로 찾는다: [05 §3](05-observability-and-analytics.md#3-추적-traces)).

## 2. 예약 API (`/v1`, 클라이언트용)

인증: `X-Api-Key` 헤더.

### 빈 시간 조회
```
GET /v1/providers
GET /v1/providers/{provider}/availability?resource_id=store-001&date=2026-11-20
```
```json
{
  "provider": "alpha",
  "resource_id": "store-001",
  "slots": [
    { "start": "2026-11-20T10:00:00Z", "end": "2026-11-20T11:00:00Z", "remaining": 3 }
  ],
  "fetched_at": "2026-11-09T12:00:00Z",
  "stale": false
}
```
- 업체 결과를 30초 동안 캐시한다 (moka).
- 업체 호출이 실패해도 캐시에 이전 값이 있으면 `"stale": true`로 돌려준다 (stale-if-error, 최대 5분).

### 예약 생성
```
POST /v1/bookings
X-Api-Key: ...
Idempotency-Key: 7c1e...   (필수, 1~255자)
```
```json
{
  "provider": "alpha",
  "resource_id": "store-001",
  "slot_start": "2026-11-20T19:00:00+09:00",
  "party_size": 4,
  "customer": { "name": "홍길동", "phone": "010-1234-5678" }
}
```
→ `202 Accepted`, `Location: /v1/bookings/{id}`
```json
{
  "id": "01933b6e-...",
  "status": "pending",
  "provider": "alpha",
  "resource_id": "store-001",
  "slot_start": "2026-11-20T10:00:00Z",
  "party_size": 4,
  "provider_booking_id": null,
  "status_reason": null,
  "created_at": "...",
  "updated_at": "..."
}
```

| 상황 | 응답 |
|---|---|
| 입력 오류, 슬롯 시작 시각이 과거 | 400 |
| API 키가 없거나 틀림 | 401 |
| 없는 업체 / 비활성 업체 | 404 / 409 |
| 같은 키 + 같은 본문 | 기존 예약을 그대로 202로 |
| 같은 키 + 다른 본문 | 422 |

### 예약 조회와 취소
```
GET  /v1/bookings/{id}            → 200 예약 (자기 클라이언트가 만든 것만)
POST /v1/bookings/{id}/cancel     → 202 (confirmed → cancel_requested)
```
취소는 원래 멱등이다. 이미 `cancel_requested`나 `cancelled`면 그대로 202를 돌려준다. `confirmed`가 아닌 다른 상태면 409.

### 헬스 체크
```
GET /healthz   프로세스가 살아 있음
GET /readyz    DB 연결 정상 (워커 역할이면 큐 처리도 정상)
```

## 3. 관리자 API (`/admin/api`)

인증: Phase 6 전까지는 로컬 전용(인증 없음), 그 뒤로는 세션 쿠키.

| 메서드 | 경로 | 설명 | 구현 |
|---|---|---|---|
| GET | `/bookings?status=&provider=&from=&to=&cursor=&limit=` | 예약 목록 (기간은 접수 시각 기준) | Phase 5 |
| GET | `/bookings/{id}` | 예약 상세 + 상태 이력 + 업체 호출 기록 + 작업 | Phase 5 |
| POST | `/bookings/{id}/retry` | `failed` 재시도 (`cancel_failed`는 취소 흐름과 함께) | Phase 5 (`cancel_failed`: Phase 6) |
| GET | `/providers` | 업체 목록 + 런타임 요약 | Phase 5 |
| GET | `/providers/{id}` | 정책 + 런타임 상태 (서킷, limiter 여유, pause, 동시 실행 수) | Phase 5 |
| PUT | `/providers/{id}/policy` | 정책 변경 (즉시 반영) | Phase 6 ([D-28](decisions.md#d-28-업체-정책-편집은-정책-실시간-반영phase-6과-함께-만든다)) |
| POST | `/providers/{id}/circuit` | `{ "action": "force_open" \| "force_close" \| "reset" }` | Phase 5 |
| GET | `/stats/providers/{id}/timeseries?from=&to=&interval=5m` | 구간별 호출 수, 결과별 수, p50/p95/p99 | Phase 5 |
| GET | `/stats/providers/summary?window=1h` | 업체별 요약 (대시보드 카드용) | Phase 5 |
| GET | `/jobs?status=dead&kind=&cursor=&limit=` | 작업 목록 | Phase 5 |
| GET | `/jobs/counts` | 작업 종류 · 상태별 개수 (작업 큐 화면 · 대시보드) | Phase 5 (추가) |
| POST | `/jobs/{id}/requeue` | dead 작업 재큐잉 | Phase 5 |
| GET | `/reconciliation/issues?resolved=false` | 대사 이슈 | Phase 6 |
| POST | `/reconciliation/issues/{id}/resolve` | 처리 완료 표시 | Phase 6 |
| GET / PUT | `/chaos/{provider}` | mock-provider 장애 설정 중계 (개발 환경 전용) | Phase 5 |
| POST | `/chaos/{provider}/preset/{name}` | 프리셋 적용 (개발 환경 전용) | Phase 5 |
| GET | `/info` | 관리자 화면 설정 (Grafana 주소, 장애 주입 · 런타임 상태를 쓸 수 있는가) | Phase 5 (추가) |

시계열 응답 예:
```json
{
  "provider": "alpha",
  "interval": "5m",
  "points": [
    {
      "bucket": "2026-11-09T12:00:00Z",
      "calls": 1520,
      "by_outcome": { "success": 1490, "timeout": 12, "rate_limited": 18 },
      "latency_ms": { "p50": 84, "p95": 310, "p99": 920 },
      "retries": 25,
      "min_rate_limit_remaining": 3
    }
  ]
}
```

**구현 (Phase 5)**: `crates/booking-server/src/api/admin/`. 요청 · 응답의 정확한 모양은 `/docs`(Swagger UI)가 기준이다.

- **목록과 커서**: 최근 것부터 `limit`건(기본 50, 최대 200). 다음 페이지가 있으면 `next_cursor`를 주고, 그 값을 그대로 `cursor`에 넣어 이어 읽는다 (마지막 항목의 ID다. 예약 ID는 UUID v7이라 ID 순서가 곧 접수 순서다). OFFSET 대신 커서라서 목록을 보는 동안 새 예약이 들어와도 같은 항목이 두 페이지에 나오지 않는다.
- **예약 상세**는 고객 정보(이름 · 전화번호)를 관리자 화면에도 내보내지 않는다. 호출 기록의 `error_detail`(업체가 보낸 오류 본문 등)은 원인을 찾는 용도라 그대로 준다 ([05 §5](05-observability-and-analytics.md#5-로그)의 "호출 기록에만 남긴다").
- **재시도 · 재큐잉** ([D-26](decisions.md#d-26-관리자-재시도는-예약을-잠그고-dead-작업을-같은-행으로-되살리고-기한을-다시-잡고-보내기-전에-업체에서-먼저-찾는다)): 예약 행을 잠그고 `failed → pending`, 확정 기한을 지금부터 다시 잡고, 그 예약의 dead 작업을 같은 행으로 되살린다 (`submit_booking` + `{"lookup_first": true}`. 시도 횟수는 lease 증표라 되돌리지 않고 한도를 8회 늘린다). 워커는 보내기 전에 업체에서 먼저 찾아, 이미 있으면 다시 보내지 않고 확정한다. 응답은 `202`와 바뀐 예약 · 작업.
  | 상황 | 응답 |
  |---|---|
  | `failed`가 아닌 예약, 처리 중인 작업이 있는 예약, 슬롯이 이미 시작한 예약 | 409 |
  | dead가 아닌 작업 | 409 |
  | 예약이 `failed`가 아닌데 묻힌 작업 (예약이 없음, 서버가 모르는 종류, 예약은 `pending`으로 남음 등) | 예약 상태는 그대로 두고 작업만 되살린다. `submit_booking`이면 `lookup_first`를 붙이고, 예약이 `pending`이면 기한을 다시 잡는다 |
- **업체 상세의 정책**은 지금 실제로 적용 중인 값이다 (파이프라인이 서버를 시작할 때 읽은 정책). 그 뒤 DB의 정책이 바뀌었으면 `restart_required: true`다 (실시간 반영은 Phase 6, [D-28](decisions.md#d-28-업체-정책-편집은-정책-실시간-반영phase-6과-함께-만든다)).
- **업체 런타임 상태**는 업체를 부르는 프로세스의 메모리에 있다 ([D-10](decisions.md#d-10-서킷rate-limit-상태는-프로세스-메모리에-단일-인스턴스-전제)). 워커와 같은 프로세스(`--role all`, 기본값)일 때만 보이고 조작할 수 있다. `--role api`로 띄우면 `runtime`은 `null`, 서킷 조작은 503이다.
  - `limiter_available`: 우리 rate limiter 버킷에 남은 토큰 수. governor는 토큰을 쓰지 않고 엿보는 기능이 없어서, 마지막으로 토큰을 받을 때 남은 수에 그 뒤로 다시 채워졌을 수를 더한 **추정값**이다.
  - `rate_limit_remaining`: 업체가 마지막 응답에서 알려 준 남은 호출 수 (`X-RateLimit-Remaining`).
- **서킷 조작** ([03 §7](03-resilience.md#7-서킷-브레이커)): `force_open`은 풀 때까지 열어 둔다 (`open_ms`가 지나도 시험 호출을 하지 않는다. 업체 점검 공지에 대응). `force_close`는 풀 때까지 실패가 쌓여도 열리지 않는다. `reset`은 강제 상태를 풀고 닫힌 상태에서 처음부터 다시 센다. 강제된 동안의 호출 결과는 세지 않는다.
- **통계** ([05 §7](05-observability-and-analytics.md#7-분석-쿼리-관리자-통계-api의-기반)): `interval`은 `10s` · `1m` · `5m` · `15m` · `1h`, `window`는 `5m` · `15m` · `1h` · `6h` · `24h`. 시계열은 호출이 없는 구간도 0으로 채워 돌려준다 (최대 1,000구간, 넘으면 400). 지연 분위수와 성공률은 관문에서 막혀 부르지 않은 시도(`circuit_open`, `throttled`)를 빼고 계산한다. 성공률 = (`success` + `rejected`) ÷ 실제로 부른 시도 (Grafana 대시보드와 같은 정의).
- **장애 주입 중계**는 `BOOKING_CHAOS_URL`(mock 업체 주소)을 설정할 때만 켜진다 ([D-27](decisions.md#d-27-장애-주입-중계는-booking_chaos_url을-설정할-때만-켠다)). 꺼져 있으면 503(`chaos-disabled`). mock이 거절한 설정(확률이 0~1 밖 등)은 그 사유와 함께 400, mock에 없는 업체는 404, mock을 부를 수 없으면 502.
- 관리자 API에 없는 경로는 Problem Details 형식의 404다 (관리자 웹 화면 경로 `/admin/*`보다 먼저 맞는다).
- 관리자 API 요청도 예약 API처럼 추적 · 지표(`http.server.request.duration`, 라우트 틀 `/admin/api/bookings/{id}` 등)에 남는다. 재시도 · 서킷 조작 같은 조작 요청의 trace는 상태 이력과 되살린 작업에서 이어지므로 남기는 편이 낫다. 대신 화면이 몇 초마다 부르는 조회 요청도 trace가 되므로, Tempo에서는 span 이름 · 서비스로 걸러 본다 ([05 §3](05-observability-and-analytics.md#3-추적-traces)). Service 대시보드의 "예약 API 가용성"(SLO)은 `/v1` 라우트만 센다.

## 4. Mock Provider API (포트 9090)

일부러 **성격이 다른 업체 두 곳**을 흉내 낸다. 서로 다른 API를 어댑터가 같은 도메인 모델로 옮기는 연습이 된다.

### alpha: 모범적인 REST API
| 메서드 | 경로 | 비고 |
|---|---|---|
| GET | `/alpha/v1/resources` | |
| GET | `/alpha/v1/availability?resource_id=&date=YYYY-MM-DD` | |
| POST | `/alpha/v1/reservations` | `Idempotency-Key` 지원. 201 생성 / 200 같은 키 재요청 / 409 `slot_unavailable` |
| GET | `/alpha/v1/reservations/{id}` | |
| GET | `/alpha/v1/reservations?client_reference=` | 참조값으로 조회 |
| GET | `/alpha/v1/reservations?from=&to=` | 기간별 목록 (대사용) |
| DELETE | `/alpha/v1/reservations/{id}` | |

- 시각: RFC 3339 UTC. 요청의 `slot_start`에는 `+09:00` 같은 오프셋을 붙여도 된다.
- 한도 초과: `429` + `Retry-After` (초 단위)
- 업체 API 응답에는 `X-RateLimit-Limit`(초당 허용 수), `X-RateLimit-Remaining`, `X-RateLimit-Reset`(버킷이 다시 가득 차기까지 초)이 붙는다. 한도는 토큰 버킷 방식이다.
- 자원: `store-001`(강남점), `store-002`(홍대점), `store-003`(판교점)
- 슬롯: 한국 시간 10:00~21:00, 1시간 단위 11개. 슬롯마다 정원이 0~4건이다 (0이면 마감된 슬롯).

예약 생성 요청:
```json
{
  "resource_id": "store-001",
  "slot_start": "2026-11-20T19:00:00+09:00",
  "party_size": 2,
  "customer": { "name": "홍길동", "phone": "010-1234-5678" },
  "client_reference": "우리 booking id (선택)"
}
```

| 응답 | 경우 |
|---|---|
| 201 | 새로 만듦 |
| 200 | 같은 `Idempotency-Key` + 같은 본문 (이전 결과를 그대로) |
| 400 `invalid_request` / `invalid_slot` / `slot_in_past` | 입력 오류, 슬롯 경계가 아닌 시각, 지난 슬롯 |
| 404 `resource_not_found` | 없는 자원 |
| 409 `slot_unavailable` | 정원 마감 |
| 422 `idempotency_key_mismatch` | 같은 `Idempotency-Key` + 다른 본문 |

에러 본문: `{ "error": { "code": "slot_unavailable", "message": "..." } }`

취소(`DELETE`)는 멱등이다. 이미 취소된 예약도 200으로 같은 결과를 돌려준다.

### beta: 까다로운 레거시 API
| 메서드 | 경로 | 비고 |
|---|---|---|
| GET | `/beta/api/slots?shopCode=&day=YYYYMMDD` | |
| POST | `/beta/api/book` | 멱등키 **미지원**. 본문에 `externalRef` |
| GET | `/beta/api/book/by-ref/{externalRef}` | 참조값으로 조회 |
| GET | `/beta/api/bookings?fromDay=&toDay=` | 기간별 목록 |
| POST | `/beta/api/book/{bookingNo}/cancel` | |

- 필드명은 camelCase, 시각은 **한국 시간 문자열 `yyyyMMddHHmm` (오프셋 없음)**
- 에러도 **HTTP 200**으로 온다: `{ "ok": false, "code": "FULL" }`
- 한도 초과: `503` + `Retry-After` (429가 아님)
- rate limit 헤더 없음

### 장애 주입 · 검사 API
| 메서드 | 경로 | 설명 |
|---|---|---|
| GET | `/_chaos` | 모든 업체 설정 |
| PUT | `/_chaos/{provider}` | 설정 교체 |
| POST | `/_chaos/{provider}/preset/{name}` | `healthy`, `flaky`, `slow`, `throttled`, `lossy`, `outage` |
| POST | `/_reset` | 예약 데이터와 설정 초기화 |
| GET | `/_inspect/{provider}/reservations` | 저장된 예약 전체 (테스트 검증용) |
| POST | `/_inspect/{provider}/reservations/{id}/cancel` | 업체측 임의 취소 (대사 테스트용) |

장애 설정:
```json
{
  "latency_ms": { "min": 20, "max": 120 },
  "slow":  { "probability": 0.0, "latency_ms": 3000 },
  "error": { "probability": 0.0, "statuses": [500, 502, 503] },
  "hang_probability": 0.0,
  "drop_after_commit_probability": 0.0,
  "rate_limit": { "per_second": 20, "burst": 10 },
  "outage": false
}
```
| 항목 | 효과 |
|---|---|
| `latency_ms` | 모든 요청에 이 범위의 무작위 지연 |
| `slow` | 일정 확률로 지연을 더함 |
| `error` | 일정 확률로 5xx (예약은 **만들지 않음**) |
| `hang_probability` | 처리하지 않고 30초 동안 응답하지 않음 (클라이언트 타임아웃 유발) |
| `drop_after_commit_probability` | 쓰기 요청(POST · PUT · PATCH · DELETE)만 해당. 예약은 **만들고** 30초 동안 응답하지 않음 → 결과 불명 상황 재현 |
| `rate_limit` | 토큰 버킷. 초당 `per_second`개가 다시 채워지고 최대 `burst`개까지 쌓임 |
| `outage` | 모든 요청에 503 (rate limit 토큰도 쓰지 않음) |

- `PUT /_chaos/{provider}`에서 빠진 필드는 `healthy` 값으로 채워진다. 확률이 0~1을 벗어나는 등 잘못된 값은 400.
- 적용 순서: outage → rate limit → 지연 → 5xx 또는 hang → 처리 → drop_after_commit
- 프리셋: `healthy`(20~120ms, 초당 20회), `flaky`(5xx 10%), `slow`(30% 확률로 +3초), `throttled`(초당 5회), `lossy`(쓰기의 20% 응답 유실), `outage`

슬롯 데이터는 날짜와 자원 ID로 정해지는 고정 규칙으로 만든다. 재시작해도 같은 슬롯이 나온다.

실행: `cargo run -p mock-provider` (주소는 `MOCK_PROVIDER_ADDR`로 바꿀 수 있고, 기본값은 `127.0.0.1:9090`)
