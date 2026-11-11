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
| 차트 | @mantine/charts (recharts) | Mantine 테마와 어울리는 선 · 막대 차트 |
| 검사 | TypeScript(`tsc`), ESLint (typescript-eslint, react-hooks, react-refresh) | 훅 규칙(최상단에서만 부르기, 의존성 배열)을 기계적으로 지킨다 |

버전 (Phase 5): React 19.3, Vite 8, Mantine 9.7, TanStack Query 5, React Router 8, TypeScript 5.9. `package.json`에 정확한 버전으로 고정하고, 공개된 지 1주 이상 지난 버전을 쓴다. TypeScript 7은 openapi-typescript · typescript-eslint가 아직 지원하지 않는다 ([D-29](decisions.md#d-29-관리자-웹-의존성은-정확한-버전으로-고정하고-공개된-지-1주-이상-지난-버전을-쓴다-typescript는-5x)).

## 2. 화면 구성

| 경로 | 화면 | 주요 기능 |
|---|---|---|
| `/` | 대시보드 | 업체별 카드(서킷 상태, 성공률, p95, 429 수, pause 여부), 대기 · dead 작업 수, 최근 실패한 예약, 미해결 대사 이슈 수(Phase 6). 5초마다 갱신 |
| `/bookings` | 예약 목록 | 상태 · 업체 · 기간 필터, 커서 페이지네이션 |
| `/bookings/:id` | 예약 상세 | 상태 타임라인, 업체 호출 기록 표(시도 · 결과 · 지연), trace 링크, 재시도 버튼 |
| `/providers/:id` | 업체 상세 | p50/p95/p99 시계열 차트, 결과 분포, rate limit 여유 추이, 런타임 상태, 정책(Phase 5는 읽기 전용, 편집은 Phase 6), 서킷 강제 조작 |
| `/jobs` | 작업 큐 | 종류 · 상태별 개수, dead 작업 목록과 재큐잉 |
| `/reconciliation` | 대사 이슈 | 미해결 목록, 처리 완료 표시 (Phase 6) |
| `/chaos` | 장애 주입 | 업체별 프리셋 버튼, 세부 설정 슬라이더. 설정을 바꾸고 바로 대시보드에서 반응을 확인하는 용도 |

Grafana와는 역할을 나눈다. 실시간 시계열 탐색과 알림은 Grafana가 맡고, **조작(재처리 · 정책 변경 · 장애 주입)과 예약 단위 조회**는 관리자 웹이 맡는다.

## 3. 폴더 구조

```
web/admin/
├─ package.json              # 정확한 버전으로 고정 (D-29). pnpm 버전은 packageManager
├─ pnpm-lock.yaml            # 커밋함 (CI는 --frozen-lockfile)
├─ vite.config.ts            # base /admin/, 개발 중 /admin/api → 127.0.0.1:8080 프록시
├─ eslint.config.js
├─ openapi.json              # 백엔드에서 생성 (커밋함, 직접 고치지 않음)
└─ src/
   ├─ main.tsx               # 전역 설정: Mantine 테마, QueryClient, BrowserRouter(basename /admin)
   ├─ App.tsx                # 레이아웃(머리글 + 왼쪽 메뉴) + 라우트. 화면은 lazy로 나눠 내려받는다
   ├─ api/
   │  ├─ schema.d.ts         # 자동 생성 (직접 수정 금지)
   │  ├─ client.ts           # openapi-fetch 클라이언트, unwrap(실패면 ApiError를 던짐)
   │  └─ queries/            # 화면별 TanStack Query 훅: bookings, jobs, providers, stats, chaos, info
   ├─ pages/                 # 경로별 화면: Dashboard, Bookings, BookingDetail, Provider, Jobs, Chaos, NotFound
   ├─ components/            # 공용 부품: Badges(상태 · 결과 · 서킷), TraceLink, ProviderCharts, QueryState
   └─ lib/                   # 순수 함수: format(시각 · 지연 · 비율), labels(상태별 이름 · 색), grafana(링크)
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

**구현 (Phase 5)**
- `cd web/admin && pnpm gen`이 두 단계를 한 번에 한다 (`gen:openapi` → `gen:api`). 서버 API를 바꿨으면 이 명령을 돌리고 두 파일을 함께 커밋한다.
- 검사는 `scripts/check.sh`의 두 단계다: `api`(서버 코드로 만든 문서 = 커밋된 `openapi.json`), `web`(그 문서로 만든 타입 = 커밋된 `schema.d.ts`, 그리고 타입 검사 · ESLint · 빌드). CI는 두 단계를 각각 job으로 돌린다.
- 경로마다 `operationId`를 직접 정해 둔다 (`admin_list_bookings` 등). 함수 이름을 그대로 쓰면 예약 API와 관리자 API의 `get_booking`처럼 겹친다.

## 5. 개발과 배포

| | 개발 | 배포 |
|---|---|---|
| 프론트 | `pnpm dev` (Vite :5173, HMR) | `pnpm build` → `web/admin/dist` |
| API 연결 | Vite 프록시로 `/admin/api` → :8080 | booking-server가 `/admin`에서 정적 파일 제공 (tower-http `ServeDir`, 없는 경로는 index.html로) |
| 결과 | 프로세스 두 개 | **프로세스 하나, 주소 하나** (CORS 불필요) |

**구현 (Phase 5)**
```bash
cd web/admin
pnpm install                 # 처음 한 번
pnpm dev                     # http://localhost:5173/admin/  (서버 :8080을 함께 띄워 둔다)
pnpm build                   # dist/ → 서버를 다시 띄우면 http://127.0.0.1:8080/admin/
pnpm gen                     # 서버 API를 바꾼 뒤: openapi.json + src/api/schema.d.ts 다시 만들기
pnpm typecheck && pnpm lint  # 타입 검사 · ESLint
```
- booking-server는 시작할 때 `BOOKING_ADMIN_WEB_DIR`(기본 `web/admin/dist`)에 `index.html`이 있으면 `/admin`에서 화면을 내준다. 없으면 로그로 알리고 API만 연다. `/`는 `/admin/`으로 넘긴다.
- 화면 경로(`/admin/bookings/…`)를 새로 고쳐도 `index.html`이 나오고 React Router가 화면을 고른다. 관리자 API(`/admin/api/...`)는 더 구체적인 경로라 화면 규칙보다 먼저 맞고, 없는 API 경로는 HTML이 아니라 JSON 404다.
- 화면별 코드를 처음 열 때 내려받는다 (`React.lazy`). 차트 라이브러리가 업체 상세 화면에만 실려 첫 화면이 가볍다.

## 6. 인증 (Phase 6)

- `admin_users` 테이블 + 비밀번호 해시(argon2)
- `POST /admin/api/login` → 세션 쿠키 (`HttpOnly`, `SameSite=Strict`, `Secure`), tower-sessions 사용
- 역할: `viewer`(조회만), `operator`(재처리 · 서킷 조작 · 장애 주입), `admin`(정책 변경)
- 관리자 조작은 모두 `booking_events.actor`나 별도 감사 로그에 남긴다

## 7. 구현 메모 (Phase 5)

**서버 데이터는 TanStack Query 훅으로만** (`src/api/queries/`). 화면은 `useBookingList(filters)`처럼 훅을 부르고, 로딩 · 오류 · 캐시 · 주기적 갱신은 TanStack Query가 맡는다. `useEffect` 안에서 fetch하지 않는다.

| 데이터 | 다시 읽는 간격 |
|---|---|
| 대시보드 (업체 · 요약 · 작업 수) | 5초 |
| 업체 상세 (런타임 상태) / 시계열 | 3초 / 10초 |
| 예약 상세 | 처리 중(`pending` · `unknown` · `cancel_requested`)이면 2초, 끝났으면 10초 |
| 예약 · 작업 목록 | 10초 / 5초 |
| 화면 설정 (`/admin/api/info`) | 한 번 (서버를 다시 띄우기 전에는 바뀌지 않는다) |

- **바꾸는 요청은 `useMutation`**: 성공하면 알림을 띄우고 관련 데이터를 다시 읽게 한다 (`invalidateQueries`). 재시도 · 재큐잉은 여러 화면이 함께 바뀌어서 읽어 둔 데이터를 전부 다시 읽는다.
- **오류**: `unwrap`이 실패 응답을 `ApiError`로 던지고, 화면은 Problem Details의 `detail`과 `trace_id`를 보여 준다.
- **목록 필터는 주소에 둔다** (`/bookings?status=failed`). 새로 고치거나 주소를 공유해도 같은 목록이 나오고, 대시보드의 "모두 보기"처럼 다른 화면이 필터를 정해 넘길 수 있다. 기간 필터는 고른 순간에 시작 시각을 계산해 주소에 넣는다 (화면을 그릴 때마다 계산하면 조건이 계속 바뀌어 목록을 끝없이 다시 읽는다).
- **커서 페이지네이션**은 `useInfiniteQuery` + "더 보기" 버튼.
- **trace 링크**: 예약 이력 · 호출 기록의 trace ID를 누르면 Grafana Explore(Tempo)에서 그 trace를 연다. Grafana 주소는 `/admin/api/info`의 `grafana_url`(서버 설정 `BOOKING_GRAFANA_URL`)이고, 검색 범위는 그 일이 일어난 시각 ±1시간이다.
- **짧은 ID**는 UUID의 끝 8자리다. 예약 ID는 UUID v7이라 앞부분이 생성 시각이어서, 비슷한 때 만든 예약은 앞자리가 같다.
- **장애 주입 폼**은 서버의 설정이 바뀌면(시나리오 적용) `key`를 바꿔 폼을 새로 만든다. 입력 중인 값은 폼 컴포넌트의 상태(`useState`)에 두고 저장할 때만 보낸다 (서버 값과 입력값을 `useEffect`로 맞추지 않는다).
- 상태 · 결과의 한국어 이름과 색은 `lib/labels.ts`의 `Record<타입, …>`에 모았다. 서버에 상태가 추가되면 API 타입이 바뀌어 빠진 항목이 컴파일 에러로 드러난다.
- 화면 색은 운영체제 설정을 따른다 (밝게 · 어둡게, `defaultColorScheme="auto"`).
- 업체 상세의 정책은 지금 적용 중인 값이다. 서버가 시작한 뒤 DB의 정책이 바뀌었으면(`restart_required`) 재시작이 필요하다고 알린다.
- 작업의 "시도"는 `attempts / max_attempts`다. dead 작업을 다시 넣으면 시도 횟수는 그대로 두고 한도가 늘어난다 (예: 3 / 11).

