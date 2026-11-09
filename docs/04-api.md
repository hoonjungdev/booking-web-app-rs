# 04. API 설계

## 1. 공통 규약

| 항목 | 규칙 |
|---|---|
| 형식 | JSON, 필드명 `snake_case` |
| 시각 | RFC 3339 (`2026-11-20T19:00:00+09:00`). 응답은 항상 UTC(`Z`) |
| ID | UUID v7 문자열 |
| 에러 | RFC 9457 Problem Details (`application/problem+json`). ASP.NET Core의 `ProblemDetails`와 같은 형식 |
| 페이지네이션 | 커서 방식: `?limit=50&cursor=<불투명 문자열>` → 응답에 `next_cursor` |
| 추적 | 모든 응답 헤더에 `X-Trace-Id`. 에러 본문에도 `trace_id` |
| 문서 | `/openapi.json`, `/docs` (Swagger UI). utoipa로 코드에서 생성 |

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

| 메서드 | 경로 | 설명 |
|---|---|---|
| GET | `/bookings?status=&provider=&from=&to=&cursor=` | 예약 목록 |
| GET | `/bookings/{id}` | 예약 상세 + 상태 이력 + 업체 호출 기록 |
| POST | `/bookings/{id}/retry` | `failed`/`cancel_failed` 재시도 |
| GET | `/providers` | 업체 목록 + 런타임 요약 |
| GET | `/providers/{id}` | 정책 + 런타임 상태 (서킷, limiter 여유, pause, 동시 실행 수) |
| PUT | `/providers/{id}/policy` | 정책 변경 (즉시 반영) |
| POST | `/providers/{id}/circuit` | `{ "action": "force_open" \| "force_close" \| "reset" }` |
| GET | `/stats/providers/{id}/timeseries?from=&to=&interval=5m` | 구간별 호출 수, 결과별 수, p50/p95/p99 |
| GET | `/stats/providers/summary?window=1h` | 업체별 요약 (대시보드 카드용) |
| GET | `/jobs?status=dead&kind=` | 작업 목록 |
| POST | `/jobs/{id}/requeue` | dead 작업 재큐잉 |
| GET | `/reconciliation/issues?resolved=false` | 대사 이슈 |
| POST | `/reconciliation/issues/{id}/resolve` | 처리 완료 표시 |
| GET / PUT | `/chaos/{provider}` | mock-provider 장애 설정 중계 (개발 환경 전용) |
| POST | `/chaos/{provider}/preset/{name}` | 프리셋 적용 (개발 환경 전용) |

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
