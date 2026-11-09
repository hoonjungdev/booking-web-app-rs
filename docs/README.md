# 설계 문서

## 읽는 순서

1. [01-architecture.md](01-architecture.md): 전체 구조, 기술 스택, C#↔Rust 대응표
2. [02-domain-and-data.md](02-domain-and-data.md): 예약 상태 머신, 멱등성, DB 스키마, 작업 큐
3. [03-resilience.md](03-resilience.md): 실패 분류, 재시도 · 서킷 브레이커 · rate limit, 결과 불명 처리
4. [04-api.md](04-api.md): 예약 API, 관리자 API, mock 업체 API
5. [05-observability-and-analytics.md](05-observability-and-analytics.md): 추적 · 메트릭 · 로그, p95 분석, 대시보드 · 알림
6. [06-admin-frontend.md](06-admin-frontend.md): 관리자 웹
7. [07-roadmap.md](07-roadmap.md): 단계별 구현 계획

[decisions.md](decisions.md)에는 왜 그렇게 정했는지를 기록합니다.

## 용어

| 용어 | 뜻 |
|---|---|
| Provider (업체) | 우리가 연동하는 외부 예약 시스템. 이 프로젝트에서는 mock 업체 alpha, beta |
| Adapter (어댑터) | 업체마다 다른 API를 우리 도메인 모델로 옮겨 주는 코드 |
| Job (작업) | 워커가 처리할 일 한 건. `jobs` 테이블에 저장됨 |
| Outbox | 데이터 저장과 "해야 할 일" 등록을 한 트랜잭션으로 묶는 패턴. 유실을 막음 |
| Idempotency key (멱등키) | 같은 요청을 여러 번 보내도 결과가 한 번만 생기게 하는 식별자 |
| Unknown outcome (결과 불명) | 요청을 보냈지만 성공했는지 알 수 없는 상태 (주로 타임아웃) |
| Circuit breaker (서킷 브레이커) | 실패가 계속되는 업체 호출을 잠시 차단하는 장치 |
| Bulkhead | 업체별로 동시 실행 수를 나눠, 한 업체 장애가 다른 업체로 번지지 않게 하는 격리 |
| Rate limit | 시간당 호출 수 제한 |
| Reconciliation (대사) | 우리 데이터와 업체 데이터를 비교해 어긋난 것을 찾는 작업 |
| p95 | 지연 시간을 정렬했을 때 95% 지점의 값. "20번 중 19번은 이보다 빠르다" |
| SLO | 서비스가 지키기로 한 목표 수치 (예: 예약 처리 완결률 99%) |
