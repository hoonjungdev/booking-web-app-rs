# booking-web-app-rs

외부 예약 업체 API와 연동하는 **견고한 예약 게이트웨이 서비스**입니다. Rust를 배우면서 만드는 프로젝트입니다.

## 구성

| 구성 요소 | 기술 | 역할 |
|---|---|---|
| booking-server | Rust · axum · sqlx | 예약 API, 관리자 API, 백그라운드 워커 |
| mock-provider | Rust · axum | 장애를 주입할 수 있는 가짜 외부 예약 업체 (alpha, beta) |
| 관리자 웹 | React · TypeScript · Vite · Mantine | 대시보드, 예약 · 작업 조회와 재처리, 업체 상태 · 서킷 조작 · 정책 편집, 대사 이슈, 장애 주입, 감사 로그 (로그인 · 역할별 권한) |
| 관측 | OpenTelemetry → Grafana LGTM | 추적 · 메트릭 · 로그 · 대시보드 |
| 저장소 | PostgreSQL | 예약, 작업 큐, 업체 호출 기록, 대사 이슈, 관리자 계정 · 감사 로그 |

## 로컬 실행

```bash
cp .env.example .env                       # 처음 한 번
docker compose up -d --wait                # PostgreSQL, Grafana LGTM
(cd web/admin && pnpm install && pnpm build)   # 관리자 웹 빌드 (처음 한 번, 화면을 고친 뒤)
cargo run -p mock-provider                 # 가짜 외부 업체 (http://127.0.0.1:9090)
cargo run -p booking-server -- serve       # 예약 서버 (http://127.0.0.1:8080), 다른 터미널에서

# API 키 발급 (출력된 bk_... 키를 저장)
cargo run -q -p booking-server -- create-client --name my-app

# 관리자 계정 (출력된 비밀번호를 저장. 역할: viewer | operator | admin)
cargo run -q -p booking-server -- create-admin --username boss --role admin

# 빈 시간 조회 (30초 캐시, 업체가 실패하면 5분까지 예전 값을 stale: true로)
curl -s -H "x-api-key: bk_..." \
  "http://127.0.0.1:8080/v1/providers/alpha/availability?resource_id=store-001&date=$(date -v+1d +%F)" | jq

# 내일 강남점의 빈 슬롯 하나를 골라 예약
SLOT=$(curl -s "http://127.0.0.1:9090/alpha/v1/availability?resource_id=store-001&date=$(date -v+1d +%F)" \
  | jq -r '[.slots[] | select(.remaining > 0)][0].start')
curl -i -X POST http://127.0.0.1:8080/v1/bookings \
  -H "x-api-key: bk_..." -H "idempotency-key: order-1" -H "content-type: application/json" \
  -d "{\"provider\":\"alpha\",\"resource_id\":\"store-001\",\"slot_start\":\"$SLOT\",\"party_size\":2,
       \"customer\":{\"name\":\"홍길동\",\"phone\":\"010-0000-0000\"}}"
# → 202 Accepted, Location: /v1/bookings/{id}. 잠시 뒤 GET으로 조회하면 status가 confirmed
# 멱등키가 없는 업체 beta도 같다: "provider":"beta", "resource_id":"GN01" (매장 GN01 · HD02 · PG03)

# 취소 (confirmed인 예약만. 업체 취소는 워커가 하고, 잠시 뒤 cancelled)
curl -i -X POST http://127.0.0.1:8080/v1/bookings/{id}/cancel -H "x-api-key: bk_..."

# mock 업체를 느리게 만들기 (flaky, throttled, lossy, outage, healthy도 있음)
curl -X POST http://127.0.0.1:9090/_chaos/alpha/preset/slow

# 업체 호출 기록 (시도 1번 = 1행): 재시도 · 타임아웃 · 서킷 차단이 결과별로 쌓인다
docker compose exec postgres psql -U booking -c \
  "SELECT outcome, count(*), round(avg(duration_ms)) AS avg_ms FROM provider_calls GROUP BY 1 ORDER BY 2 DESC"
```

### 관리자 웹

http://127.0.0.1:8080/admin/ 에 `create-admin`으로 만든 계정으로 로그인한다. 대시보드(업체별 서킷 · 성공률 · p95, 작업 큐, 미해결 대사 이슈), 예약 목록 · 상세(상태 이력, 업체 호출 기록, trace 링크, 재시도 · 취소 재시도), 업체 상세(지연 분위수 · 결과 분포 차트, 서킷 강제 조작, AIMD 허용 속도, 정책 편집, 지금 대사), 작업 큐(dead 작업 다시 넣기), 대사 이슈, 장애 주입, 감사 로그를 쓴다. 역할은 viewer(조회) < operator(재처리 · 서킷 · 장애 주입 · 대사) < admin(정책 변경)이다. API 문서는 http://127.0.0.1:8080/docs/ (Swagger UI).

- 장애 주입 화면은 `.env`에 `BOOKING_CHAOS_URL=http://127.0.0.1:9090`이 있어야 켜진다 (`.env.example`에 들어 있다. 예전에 복사한 `.env`라면 이 줄을 더한다).
- 화면을 고치는 동안은 `cd web/admin && pnpm dev`로 http://localhost:5173/admin/ 을 연다 (API는 :8080 서버로 넘어간다).
- 해 볼 것: 장애 주입에서 "완전 장애" → 위의 curl로 예약 몇 건 → 대시보드에서 서킷이 "열림"이 되고 작업이 "미룸"으로 쌓인다 → "정상"으로 되돌리면 밀린 예약이 처리된다. 확정 기한(30분)이 지나 실패한 예약은 작업 큐의 "다시 넣기"나 예약 상세의 "재시도"로 다시 처리한다 (업체에서 먼저 찾아본 뒤 없을 때만 다시 보낸다).
- 해 볼 것 (Phase 6): beta에 "응답 유실"을 걸고 beta 예약 몇 건 → 일부가 결과 불명(`unknown`)을 거쳐 조회로 확정되고, 업체 예약은 참조값마다 한 건이다 (`curl localhost:9090/_inspect/beta/reservations`). mock에서 업체측 취소(`curl -X POST localhost:9090/_inspect/beta/reservations/<BK번호>/cancel`) → 대사 이슈 화면의 "Beta (mock) 대사" → 예약이 `cancelled`로 바뀌고 처리 완료 이슈가 남는다. 장애 주입에서 alpha의 호출 한도를 낮추면 업체 상세의 허용 속도가 절반씩 줄었다가 되돌아간다 (AIMD).
- 로컬 http에서도 로그인 쿠키(`Secure`)가 동작하는 것은 브라우저가 localhost를 예외로 두기 때문이다. 로그인이 유지되지 않으면 `.env`에 `BOOKING_ADMIN_COOKIE_SECURE=false`.

### Grafana에서 보기

두 서버는 추적 · 메트릭 · 로그를 OpenTelemetry로 내보낸다. http://localhost:3000 (로그인 없음)의 Dashboards → **Booking** 폴더에 대시보드 세 개가 있다.

- **Provider Overview**: 업체별 호출량, 성공률, p50/p95/p99, 호출 제한, 서킷 상태, AIMD 허용 속도. 아래쪽 "느린 업체 호출" 표에서 행을 누르면 그 호출의 trace가 열리고, mock 업체 내부에서 지연된 구간(`chaos.delay`)까지 이어서 볼 수 있다.
- **Booking Pipeline**: 상태 전이 흐름, 접수 → 확정 시간, 대기 · dead 작업 수, 미해결 대사 이슈 (이중 예약 의심은 긴급 알림)
- **Service**: API 요청 수 · 오류 비율 · 응답 시간, tokio 런타임, 경고 · 오류 로그

응답 헤더의 `X-Trace-Id`나 예약 ID로 trace를 찾을 수 있다 (Explore → Tempo → `{ span.booking_id = "<예약 ID>" }`). 대시보드와 알림 규칙은 [observability/](observability/README.md)에서 코드로 관리한다.

품질 검사는 `scripts/check.sh`로 돌립니다 (CI와 같은 명령, PostgreSQL · pnpm 필요).

## 문서

설계 문서는 [docs/README.md](docs/README.md)에서 시작하세요.

## 진행 상태

Phase 6 (두 번째 업체 beta와 운영 기능: 업체별 워커 격리, 멱등키 없는 업체의 "보냄" 표시, 취소 흐름, 대사와 이슈 화면, 정책 실시간 변경, 빈 시간 캐시, 적응형 rate limit, 관리자 인증 · 권한 · 감사 로그)까지 진행했습니다. 단계별 계획은 [docs/07-roadmap.md](docs/07-roadmap.md)에 있습니다.
