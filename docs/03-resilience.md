# 03. 장애 대응 설계 (Resilience)

외부 업체는 우리가 통제할 수 없다. 이 문서는 업체가 느리거나, 에러를 내거나, 호출을 제한하거나, 응답 없이 끊길 때 서비스가 어떻게 움직이는지 정한다.

지켜야 할 불변 조건:
1. **이중 예약 0건**: 업체에 같은 예약이 두 번 생기지 않는다.
2. **유실 0건**: 접수한 예약은 반드시 종료 상태나 관리자 확인 대기 상태(`failed`, `cancel_failed`)에 도달한다.
3. **격리**: 한 업체의 장애가 다른 업체 호출을 막지 않는다.

## 1. 실패 분류

어댑터는 업체 응답을 아래 enum으로 분류해서 돌려준다. 그다음의 모든 판단(재시도할지, 서킷에 실패로 셀지, 어떤 상태로 보낼지)은 이 분류만 보고 한다.

```rust
pub enum ProviderError {
    /// 응답 시간 초과. 요청이 업체에 닿았는지 알 수 없다
    Timeout,
    /// 연결 실패. 요청이 업체에 닿지 않았다
    Connect,
    /// 업체가 호출 제한을 알림 (alpha: 429, beta: 503 + Retry-After)
    RateLimited { retry_after: Option<Duration> },
    /// 업체 서버 오류 (5xx)
    Server { status: u16 },
    /// 업무적 거절 (슬롯 마감 등). 기술적으로는 정상 응답
    Rejected { reason: RejectReason },
    /// 예상하지 못한 4xx. 대개 우리 쪽 버그
    Client { status: u16, body: String },
    /// 응답 형식이 계약과 다름
    Decode { detail: String },
    /// 서킷이 열려 있어서 호출하지 않음
    CircuitOpen { retry_at: Instant },
    /// 우리 쪽 rate limiter나 bulkhead에서 대기 한도를 넘음
    Throttled { retry_at: Instant },
}
```

| 분류 (`outcome`) | 호출 내 재시도 | 서킷 브레이커 집계 | 비고 |
|---|---|---|---|
| `success` | - | 성공 | |
| `rejected` | ❌ | 성공 | 업무적 거절은 업체가 정상이라는 뜻 |
| `timeout` | 멱등 안전할 때만 | 실패 | §5 참고 |
| `connect_error` | ✅ | 실패 | 요청이 가지 않았으므로 항상 안전 |
| `server_error` | 멱등 안전할 때만 | 실패 | 5xx여도 업체가 처리했을 가능성이 있음 |
| `rate_limited` | Retry-After ≤ 1초일 때만 | 제외 | 업체 pause 설정 (§6) |
| `client_error` | ❌ | 제외 | 알림 대상 (우리 버그 가능성) |
| `decode_error` | ❌ | 실패 | 알림 대상 (업체 계약 변경 가능성) |
| `circuit_open` | ❌ | - | 작업 수준에서 재예약 |
| `throttled` | ❌ | - | 작업 수준에서 재예약 |

"멱등 안전"하다는 것은 조회 호출(availability, find, list)이거나, 업체가 멱등키를 지원하는 생성 · 취소 호출이라는 뜻이다.

## 2. 파이프라인 구성

어댑터는 "HTTP ↔ 도메인 타입 변환"만 한다. 장애 대응은 어댑터를 감싸는 `ResilientProvider`가 맡는다. Polly 정책으로 HttpClient를 감싸는 것과 같은 구조다.

```
ResilientProvider::call(operation)
│
├─ [업체 pause 확인]    Retry-After로 쉬는 중이면 바로 Throttled
├─ [전체 시간 예산]      작업 1회당 최대 15초
└─ [재시도 루프]         최대 3회, 지수 백오프 + full jitter
    └─ 시도할 때마다:
        ├─ [서킷 브레이커]   열려 있으면 바로 CircuitOpen
        ├─ [rate limiter]    토큰을 기다림 (최대 500ms, 넘으면 Throttled)
        ├─ [bulkhead]        업체별 동시 실행 수 제한 (최대 200ms 대기)
        ├─ [시도 타임아웃]   3초
        ├─ 어댑터 호출 → 결과 분류
        └─ 기록: provider_calls, 메트릭, 서킷 상태 갱신
```

Polly가 권장하는 순서(Retry → CircuitBreaker → Timeout)와 같은 이유로 이 순서를 쓴다. 재시도가 가장 바깥에 있어야 시도마다 서킷과 타임아웃을 따로 거친다.

## 3. 업체별 정책 (기본값)

`providers.policy`(jsonb)에 저장한다. 관리자 화면에서 바꾸면 재시작 없이 반영된다(arc-swap으로 교체).

```json
{
  "attempt_timeout_ms": 3000,
  "call_budget_ms": 15000,
  "retry":           { "max_attempts": 3, "base_delay_ms": 200, "max_delay_ms": 2000 },
  "rate_limit":      { "per_second": 10, "burst": 5, "max_wait_ms": 500 },
  "bulkhead":        { "max_concurrency": 8, "max_wait_ms": 200 },
  "circuit_breaker": { "window": 20, "min_calls": 10, "failure_rate": 0.5,
                       "open_ms": 30000, "half_open_probes": 3 },
  "availability_cache_ttl_ms": 30000
}
```

## 4. 두 단계 재시도

| 단계 | 담당 | 간격 | 횟수 | 다루는 실패 |
|---|---|---|---|---|
| 호출 내 재시도 | ResilientProvider | 200ms → 최대 2초 (jitter) | 3회 | 짧은 일시 오류 (연결 끊김, 순간적인 5xx) |
| 작업 재예약 | 워커 | 5초 → 최대 10분 (지수 + jitter) | 8회 | 호출 내 재시도 소진, 서킷 열림, 업체 pause |

- 서킷 열림이나 pause 때문에 **호출하지 않고** 미룬 경우는 작업 시도 횟수로 세지 않는다. 대신 예약의 `deadline_at`(기본값: 접수 후 30분, 단 슬롯 시작 시각을 넘지 않음)이 지나면 `failed`로 끝낸다.
- 백오프 공식 (full jitter): `delay = random(0, min(max_delay, base × 2^(attempt-1)))`

## 5. 결과 불명 (unknown outcome) 처리

가장 위험한 경우는 예약 생성 요청을 보냈는데 타임아웃이 난 상황이다. 업체에 예약이 **생겼을 수도, 안 생겼을 수도** 있다.

```mermaid
flowchart TD
  T["create_booking 타임아웃 / 5xx"] --> Q{"업체가 멱등키를 지원하나?"}
  Q -- "예 (alpha)" --> R["같은 멱등키로 재시도<br/>업체가 중복을 막아 줌"]
  Q -- "아니오 (beta)" --> U["예약 → unknown<br/>resolve_unknown 작업 등록"]
  U --> F{"external_ref로 조회"}
  F -- "있음" --> C["confirmed"]
  F -- "없음 (30초 간격으로 2번 확인)" --> P["pending으로 되돌리고 다시 제출"]
  F -- "조회 실패" --> RR["작업 재예약<br/>횟수 소진 시 failed"]
```

"없음"을 한 번만 보고 다시 제출하지 않는 이유는, 업체 내부 처리가 늦어서 조회에 아직 안 보일 수 있기 때문이다. 두 번 확인해서 잘못된 판단을 줄인다.

어댑터는 자신이 할 수 있는 일을 선언한다.
```rust
pub struct Capabilities {
    pub idempotency_key: bool,     // 생성 · 취소에 멱등키 지원
    pub lookup_by_reference: bool, // 우리 참조값으로 예약 조회 가능
    pub list_bookings: bool,       // 기간별 예약 목록 (대사용)
}
```
멱등키도 참조값 조회도 지원하지 않는 업체는, 결과 불명이 생기면 자동으로 다시 제출하지 않고 바로 관리자 확인 대상으로 넘긴다.

## 6. Rate limit 관리

| 단계 | 방법 | 목적 |
|---|---|---|
| ① 우리가 먼저 제한 | governor (GCRA 토큰 버킷), 업체별 | 429를 아예 받지 않도록 |
| ② 업체 신호 따르기 | 429/503의 `Retry-After` → 업체 전체 **pause** (그동안 그 업체 호출 중단) | 업체에 추가 부하를 주지 않음 |
| ③ 여유량 관찰 | `X-RateLimit-Remaining` 헤더를 기록하고 메트릭으로 노출 | 관리자 화면에서 쿼터 여유 확인 |
| ④ 적응형 조절 (Phase 6) | 429를 받으면 허용 속도를 절반으로, 한동안 문제없으면 조금씩 회복 (AIMD) | 업체 한도를 몰라도 자동으로 맞춤 |

업체 한도는 여러 작업이 나눠 쓰는 자원이다. 사용자가 기다리는 조회(availability)가 백그라운드 작업(대사)에 밀리지 않도록, 대사 작업은 토큰이 50% 넘게 남아 있을 때만 호출한다 (Phase 6).

## 7. 서킷 브레이커

```mermaid
stateDiagram-v2
  [*] --> Closed
  Closed --> Open: 최근 20회 중 실패율 50% 이상 (최소 10회)
  Open --> HalfOpen: 30초 경과
  HalfOpen --> Closed: 시험 호출 3회 모두 성공
  HalfOpen --> Open: 시험 호출 중 1회라도 실패
```

- 업체마다 하나씩 프로세스 메모리에 둔다.
- 상태가 바뀔 때마다 로그, 메트릭, 업체 런타임 상태 API에 반영한다.
- 관리자가 강제로 열고 닫을 수 있다 (업체 점검 공지에 대응).
- 학습을 위해 라이브러리 대신 직접 구현한다. → [D-05](decisions.md#d-05-장애-대응은-작은-부품을-조합하고-서킷-브레이커와-재시도는-직접-구현)

## 8. Bulkhead (격리)

업체마다 `tokio::sync::Semaphore`를 하나씩 두고 동시 호출 수를 제한한다. alpha가 느려져 호출이 쌓여도 beta 쪽 자원은 그대로 남는다. 워커도 업체별 동시 처리 수를 같은 값으로 제한한다.

## 9. 대사 (Reconciliation)

정기 작업(`reconcile_provider`, 업체마다 10분 간격)이 최근 24시간 슬롯의 예약을 업체 목록과 비교한다.

| 발견한 것 | 처리 |
|---|---|
| 우리는 `confirmed`, 업체에서는 취소됨 | 예약 → `cancelled` (actor: reconciler), 이슈 기록 |
| 우리는 `confirmed`, 업체에는 없음 | 이슈 기록 (자동 처리하지 않음) |
| 우리 참조값을 단 예약이 업체에 2건 이상 | 이슈 기록 (이중 예약 의심, 긴급 알림) |
| 우리는 `failed`/`unknown`, 업체에서는 확정됨 | 이슈 기록, 예약 → `confirmed` |

## 10. 장애 시나리오와 기대 동작

mock-provider의 장애 주입으로 아래 시나리오를 자동화된 통합 테스트로 만든다. 모든 시나리오에서 **"업체에 같은 참조값의 예약이 2건 이상 없다"**를 검사한다.

| 시나리오 | mock 설정 | 기대 동작 |
|---|---|---|
| 정상 | `healthy` | 모두 `confirmed`, p95 < 300ms |
| 간헐적 5xx | 오류율 10% | 호출 내 재시도로 흡수. 재시도 메트릭은 늘지만 최종적으로 모두 `confirmed` |
| 지연 폭증 | 지연 2~5초 | 타임아웃 → 재시도 → 서킷 Open → 작업 재예약. 회복되면 Half-Open → Closed, 밀린 예약 처리 |
| 호출 제한 | 초당 5회 | 우리 limiter가 대부분 막음. 429가 오면 pause했다가 재개 |
| 커밋 후 응답 유실 | 유실 확률 20% | alpha: 같은 멱등키로 재시도해서 중복 없음. beta: `unknown` → 조회 → `confirmed` |
| 완전 장애 | `outage` | 서킷 Open, 예약은 `pending`으로 유지. 복구되면 자동 처리, 기한이 지나면 `failed` |
| 업체측 취소 | `/_inspect`로 임의 취소 | 대사 작업이 찾아내서 `cancelled`, 이슈 기록 |
| 한 업체만 장애 | alpha만 `outage` | beta 예약의 처리량과 지연에 영향 없음 (격리) |
