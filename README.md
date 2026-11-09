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

## 문서

설계 문서는 [docs/README.md](docs/README.md)에서 시작하세요.

## 진행 상태

Phase 0 (설계) 단계입니다. 단계별 계획은 [docs/07-roadmap.md](docs/07-roadmap.md)에 있습니다.
