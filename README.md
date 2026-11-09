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
docker compose up -d --wait       # PostgreSQL, Grafana LGTM
cargo run -p mock-provider        # 가짜 외부 업체 (http://127.0.0.1:9090)

# 예: 내일 강남점 빈 시간 조회
curl "http://127.0.0.1:9090/alpha/v1/availability?resource_id=store-001&date=$(date -v+1d +%F)"
# 예: 응답을 느리게 만들기
curl -X POST http://127.0.0.1:9090/_chaos/alpha/preset/slow
```

품질 검사는 `scripts/check.sh`로 돌립니다 (CI와 같은 명령).

## 문서

설계 문서는 [docs/README.md](docs/README.md)에서 시작하세요.

## 진행 상태

Phase 1 (뼈대 + mock 업체 alpha)까지 진행했습니다. 단계별 계획은 [docs/07-roadmap.md](docs/07-roadmap.md)에 있습니다.
