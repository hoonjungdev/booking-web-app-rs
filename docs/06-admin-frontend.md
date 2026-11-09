# 06. 관리자 웹

## 1. 기술 스택

| 영역 | 선택 | 이유 |
|---|---|---|
| 빌드 | Vite + React + TypeScript | 표준 조합, 빠른 개발 서버 |
| 패키지 매니저 | pnpm | 이미 설치되어 있음 (mise) |
| UI 컴포넌트 | Mantine | 테이블 · 폼 · 모달 · 알림 같은 관리자 화면 부품이 많고 차트(@mantine/charts)도 들어 있다. React 초급자도 조립만으로 화면을 만들 수 있다 |
| 서버 상태 | TanStack Query | API 호출, 캐시, 주기적 갱신, 로딩 · 에러 상태를 대신 관리한다. `useEffect`로 직접 fetch하지 않는다 |
| 라우팅 | React Router | |
| API 타입 | openapi-typescript + openapi-fetch | Rust 코드 → OpenAPI → TypeScript 타입을 자동 생성. 백엔드가 바뀌면 프론트에서 컴파일 에러로 바로 드러난다 |

## 2. 화면 구성

| 경로 | 화면 | 주요 기능 |
|---|---|---|
| `/` | 대시보드 | 업체별 카드(서킷 상태, 성공률, p95, 429 수, pause 여부), 대기 · dead 작업 수, 미해결 대사 이슈 수. 5초마다 갱신 |
| `/bookings` | 예약 목록 | 상태 · 업체 · 기간 필터, 커서 페이지네이션 |
| `/bookings/:id` | 예약 상세 | 상태 타임라인, 업체 호출 기록 표(시도 · 결과 · 지연), trace 링크, 재시도 버튼 |
| `/providers/:id` | 업체 상세 | p50/p95/p99 시계열 차트, 결과 분포, rate limit 여유 추이, 런타임 상태, 정책 편집 폼, 서킷 강제 조작 |
| `/jobs` | 작업 큐 | 종류 · 상태별 개수, dead 작업 목록과 재큐잉 |
| `/reconciliation` | 대사 이슈 | 미해결 목록, 처리 완료 표시 |
| `/chaos` | 장애 주입 | 업체별 프리셋 버튼, 세부 설정 슬라이더. 설정을 바꾸고 바로 대시보드에서 반응을 확인하는 용도 |

Grafana와는 역할을 나눈다. 실시간 시계열 탐색과 알림은 Grafana가 맡고, **조작(재처리 · 정책 변경 · 장애 주입)과 예약 단위 조회**는 관리자 웹이 맡는다.

## 3. 폴더 구조

```
web/admin/
├─ package.json
├─ vite.config.ts            # 개발 중 /admin/api → localhost:8080 프록시
├─ openapi.json              # 백엔드에서 생성 (커밋함)
└─ src/
   ├─ main.tsx
   ├─ App.tsx                # 레이아웃(사이드바) + 라우트
   ├─ api/
   │  ├─ schema.d.ts         # 자동 생성 (직접 수정 금지)
   │  ├─ client.ts           # openapi-fetch 클라이언트
   │  └─ queries/            # 화면별 TanStack Query 훅 (useBookings, useProviderStats …)
   ├─ pages/                 # 경로별 화면
   └─ components/            # 공용 부품 (StatusBadge, LatencyChart, CircuitIndicator …)
```

## 4. 타입 생성 흐름

```
Rust 핸들러 + #[utoipa::path] / #[derive(ToSchema)]
   │  cargo run -p booking-server -- openapi > web/admin/openapi.json
   ▼
openapi.json
   │  pnpm gen:api   (openapi-typescript)
   ▼
src/api/schema.d.ts  →  openapi-fetch가 경로 · 파라미터 · 응답 타입을 강제
```
CI에서는 생성 결과가 커밋된 파일과 다르면 실패시킨다.

## 5. 개발과 배포

| | 개발 | 배포 |
|---|---|---|
| 프론트 | `pnpm dev` (Vite :5173, HMR) | `pnpm build` → `web/admin/dist` |
| API 연결 | Vite 프록시로 `/admin/api` → :8080 | booking-server가 `/admin`에서 정적 파일 제공 (tower-http `ServeDir`, 없는 경로는 index.html로) |
| 결과 | 프로세스 두 개 | **프로세스 하나, 주소 하나** (CORS 불필요) |

## 6. 인증 (Phase 6)

- `admin_users` 테이블 + 비밀번호 해시(argon2)
- `POST /admin/api/login` → 세션 쿠키 (`HttpOnly`, `SameSite=Strict`, `Secure`), tower-sessions 사용
- 역할: `viewer`(조회만), `operator`(재처리 · 서킷 조작 · 장애 주입), `admin`(정책 변경)
- 관리자 조작은 모두 `booking_events.actor`나 별도 감사 로그에 남긴다
