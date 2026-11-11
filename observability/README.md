# observability

Grafana 대시보드와 알림 규칙을 코드로 관리한다. `docker compose up -d --wait`로 뜨는 `grafana/otel-lgtm` 컨테이너가 시작할 때 이 폴더의 파일을 읽어 등록한다 (provisioning). 설계는 [docs/05-observability-and-analytics.md](../docs/05-observability-and-analytics.md)에 있다.

```
observability/grafana/
├─ dashboards/                      # 대시보드 JSON (Grafana의 Booking 폴더에 등록된다)
│  ├─ provider-overview.json        #   업체 호출: 호출량, 성공률, p50/p95/p99, 호출 제한, 서킷, AIMD 허용 속도, 느린 호출의 trace
│  ├─ booking-pipeline.json         #   예약 흐름: 상태 전이, 접수 → 종료 시간, 작업 큐, 미해결 대사 이슈, 포기한 작업의 trace
│  └─ service.json                  #   HTTP 요청 수 · 오류 비율 · 응답 시간, tokio 런타임, 경고 · 오류 로그
└─ provisioning/
   ├─ dashboards/booking.yaml       # "dashboards/ 폴더의 JSON을 읽어라"는 설정
   └─ alerting/booking.yaml         # 알림 규칙 7개 (Phase 6: 대사에서 이중 예약 의심 발견, 긴급)
```

## 보는 법

| 무엇 | 어디서 |
|---|---|
| 대시보드 | http://localhost:3000 → Dashboards → **Booking** |
| 알림 규칙과 상태 | Alerting → Alert rules → **Booking** |
| trace 검색 | Explore → Tempo. 예: `{ span.booking_id = "<예약 ID>" }` |
| 로그 검색 | Explore → Loki. 예: `{service_name="booking-server"} \| detected_level="warn"` |
| 지표 직접 조회 | Explore → Prometheus. 예: `provider_circuit_state` |

장애를 주입하고 대시보드가 어떻게 반응하는지 보는 것이 가장 빠른 공부다.

```bash
curl -X POST localhost:9090/_chaos/alpha/preset/slow     # 30% 확률로 +3초 → 타임아웃 · 재시도
curl -X POST localhost:9090/_chaos/alpha/preset/outage   # 모든 요청에 503 → 서킷 Open
curl -X POST localhost:9090/_chaos/alpha/preset/healthy  # 복구
curl -X POST localhost:9090/_chaos/beta/preset/lossy     # beta(멱등키 없음): 쓰기의 20% 응답 유실 → 결과 불명 → 조회로 확정
```

| 프리셋 | Provider Overview에서 볼 것 |
|---|---|
| `slow` | 호출 지연 p95 · p99가 3초로 붙고 `timeout`이 늘어난다. "느린 업체 호출" 표에서 행을 누르면 mock 내부의 `chaos.delay`까지 보인다 |
| `flaky` | 결과별 시도 수에 `server_error`가 섞이고 "호출 내 재시도"가 늘어난다. 성공률은 조금만 떨어진다 (재시도가 흡수) |
| `throttled` | "호출 제한" 패널에 `remote`(업체의 429), 이어서 `local`(pause 중이라 부르지 않음)이 나타난다. 업체 pause가 "쉬는 중"이 된다. "적응형 rate limit (AIMD)" 줄에서 우리 limiter의 허용 속도가 절반으로 꺾였다가 계단처럼 올라간다 |
| `outage` | 서킷 상태가 Open이 되고 `circuit_open`이 대부분을 차지한다. Booking Pipeline에서는 대기 작업이 쌓였다가 복구 뒤에 빠진다 |

## 대시보드를 고칠 때

JSON 파일이 원본이다. Grafana는 파일을 10초마다 다시 읽는다.

1. Grafana 화면에서 패널을 고쳐 본다 (저장해도 되지만, 파일이 다시 읽히면 파일 내용으로 돌아간다).
2. 마음에 들면 대시보드의 Export → **Export as JSON**으로 내려받아 `dashboards/`의 파일을 덮어쓴다.
3. 새 대시보드는 `dashboards/`에 JSON 파일을 추가하면 된다. `uid`는 겹치지 않게 짓는다.

패널 질의에서 지킬 것:
- **부르지 않은 시도는 뺀다.** `provider_call_duration_seconds`에는 관문에서 막힌 시도(`circuit_open`, `throttled`)도 들어 있다. 지연 분위수 · 호출 수 · 성공률을 계산할 때는 `outcome!~"circuit_open|throttled"`를 붙인다. 빼지 않으면 0초에 가까운 값이 섞여 실제보다 빨라 보인다.
- **창은 1분 이상.** 지표를 10초마다 내보내므로(`OTEL_METRIC_EXPORT_INTERVAL`) `rate(…[1m])`이면 표본이 6개쯤 들어간다. 내보내는 간격을 늘렸다면 창도 그 4배 이상으로 늘린다.

## 알림 규칙을 고칠 때

`provisioning/alerting/booking.yaml`을 고친 뒤 컨테이너를 다시 만든다 (알림 규칙은 시작할 때만 읽는다).

```bash
docker compose up -d --force-recreate otel-lgtm
```

규칙 하나는 "A: Prometheus 질의 → C: 기준과 비교" 두 단계다. 기준을 넘은 상태가 `for` 동안 이어지면 Firing이 된다. 새 규칙을 만들 때는 Grafana 화면에서 만들어 본 뒤 Export(YAML)한 내용을 파일에 옮기는 편이 쉽다.

"지금 값"을 보는 지표로 알림을 만들 때는 그 값이 **언제 바뀌는지**를 먼저 확인한다. 예를 들어 서킷 상태는 다음 호출이 올 때에야 Open에서 벗어나므로, 상태 값만 보는 알림은 업체가 복구된 뒤에도 (처리할 예약이 없는 동안) 계속 울린다. 그래서 서킷 알림은 "열려 있고, 서킷에 막힌 호출이 계속 있다"를 조건으로 삼았다.

로컬에는 알림을 받을 곳을 연결하지 않았다. 울린 알림은 Alerting → Alert rules 화면에서 본다.

## 지표 이름

코드에서는 OpenTelemetry 이름(점으로 구분)을 쓰고, Prometheus에서는 다음처럼 바뀐다.

| 코드 | Prometheus |
|---|---|
| `provider.call.duration` (histogram, 단위 `s`) | `provider_call_duration_seconds_bucket` · `_count` · `_sum` |
| `provider.call.retries` (counter) | `provider_call_retries_total` |
| `provider.circuit.state` (gauge) | `provider_circuit_state` |
| `provider.rate_limit.allowed` (gauge, Phase 6) | `provider_rate_limit_allowed` |
| `reconciliation.issues.open` (gauge, Phase 6) | `reconciliation_issues_open` |
| 라벨 `http.route` | `http_route` |
| 서비스 이름 (`service.name`) | `job` 라벨 (`booking-server`, `mock-provider`) |

전체 목록은 [docs/05 §4](../docs/05-observability-and-analytics.md#4-메트릭-목록)에 있다. 지표를 추가하거나 라벨을 바꾸면 그 표, 여기의 대시보드 · 알림, 통합 테스트(`crates/booking-server/tests/telemetry.rs`)를 함께 고친다.

## 알아 둘 점

- **컨테이너를 다시 만들면 쌓인 데이터가 사라진다.** otel-lgtm은 추적 · 메트릭 · 로그를 컨테이너 안에 저장한다 (볼륨 없음). 로컬 실험용이라 그대로 두었다.
- **처음 나타난 시계열의 첫 값은 증가로 잡히지 않는다.** Prometheus는 시계열(라벨 조합)이 처음 보일 때의 값을 기준점으로 삼는다. 그래서 서버를 띄우고 한참 뒤에 어떤 결과(`outcome="server_error"` 등)가 처음 생기면, 그 첫 10초치는 `rate()` · `increase()` 그래프에 나타나지 않고 그다음 증가분부터 보인다. 장애를 주입하고 요청을 한 번만 보냈는데 그래프가 조용하면 한 번 더 보내 본다 (호출 기록 `provider_calls`에는 빠짐없이 있다). 두 가지로 줄여 두었다.
  - 재시도 · 호출 제한 · 버린 기록 counter는 서버가 시작할 때 0으로 만들어 둔다. 첫 증가부터 잡힌다.
  - `docker-compose.yml`이 Prometheus에 `created-timestamp-zero-ingestion` 기능을 켠다. 서버가 지표와 함께 보내는 "계측기가 시작한 시각"에 0을 넣어 주므로, **서버를 띄운 직후**의 첫 요청들은 그래프에 잡힌다. 그 시각은 서버 기준이지 시계열 기준이 아니어서, 한참 뒤에 처음 나타난 시계열에는 효과가 없다.
- **서킷은 다음 호출이 올 때 Open → Half-Open으로 바뀐다.** 처리할 예약이 없으면 업체가 복구된 뒤에도 Open으로 남아 있다가, 새 예약이 들어오면 시험 호출을 거쳐 닫힌다.
- **수집기가 꺼져 있어도 서버는 정상으로 돈다.** 내보내기 실패는 서버 콘솔에 경고로만 남는다. 아예 내보내지 않으려면 `OTEL_SDK_DISABLED=true`로 실행한다.
