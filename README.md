# booking-web-app-rs

외부 예약 업체 API와 연동하는 **견고한 예약 게이트웨이 서비스**입니다. Rust를 배우면서 만드는 프로젝트입니다.

## 구성

| 구성 요소 | 기술 | 역할 |
|---|---|---|
| booking-server | Rust · axum · sqlx | 예약 API, 관리자 API, 백그라운드 워커 |
| mock-provider | Rust · axum | 장애를 주입할 수 있는 가짜 외부 예약 업체 (alpha, beta) |
| 관리자 웹 | React · TypeScript · Vite | 대시보드, 예약 재처리, 업체 정책 편집, 장애 주입 |
| 관측 | OpenTelemetry → Grafana LGTM | 추적 · 메트릭 · 로그 · 대시보드 |
| 저장소 | PostgreSQL | 예약, 작업 큐, 업체 호출 기록 |

## 로컬 실행

```bash
cp .env.example .env                       # 처음 한 번
docker compose up -d --wait                # PostgreSQL, Grafana LGTM
cargo run -p mock-provider                 # 가짜 외부 업체 (http://127.0.0.1:9090)
cargo run -p booking-server -- serve       # 예약 서버 (http://127.0.0.1:8080), 다른 터미널에서

# API 키 발급 (출력된 bk_... 키를 저장)
cargo run -q -p booking-server -- create-client --name my-app

# 내일 강남점의 빈 슬롯 하나를 골라 예약
SLOT=$(curl -s "http://127.0.0.1:9090/alpha/v1/availability?resource_id=store-001&date=$(date -v+1d +%F)" \
  | jq -r '[.slots[] | select(.remaining > 0)][0].start')
curl -i -X POST http://127.0.0.1:8080/v1/bookings \
  -H "x-api-key: bk_..." -H "idempotency-key: order-1" -H "content-type: application/json" \
  -d "{\"provider\":\"alpha\",\"resource_id\":\"store-001\",\"slot_start\":\"$SLOT\",\"party_size\":2,
       \"customer\":{\"name\":\"홍길동\",\"phone\":\"010-0000-0000\"}}"
# → 202 Accepted, Location: /v1/bookings/{id}. 잠시 뒤 GET으로 조회하면 status가 confirmed

# mock 업체를 느리게 만들기
curl -X POST http://127.0.0.1:9090/_chaos/alpha/preset/slow
```

품질 검사는 `scripts/check.sh`로 돌립니다 (CI와 같은 명령, PostgreSQL 필요).

## 문서

설계 문서는 [docs/README.md](docs/README.md)에서 시작하세요.

## 진행 상태

Phase 2 (예약 API + DB + 워커)까지 진행했습니다. 단계별 계획은 [docs/07-roadmap.md](docs/07-roadmap.md)에 있습니다.
