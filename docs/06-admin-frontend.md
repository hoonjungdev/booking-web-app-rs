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
| (로그인 전) | 로그인 | 아이디 · 비밀번호. 로그인하지 않았으면 어느 주소로 와도 이 화면만 보인다 (Phase 6) |
| `/` | 대시보드 | 업체별 카드(서킷 상태, 성공률, p95, 429 수, pause 여부, 우리 limiter의 허용 속도), 대기 · dead 작업 수, 최근 실패한 예약, 종류별 미해결 대사 이슈 수(Phase 6). 5초마다 갱신 |
| `/bookings` | 예약 목록 | 상태 · 업체 · 기간 필터, 커서 페이지네이션 |
| `/bookings/:id` | 예약 상세 | 상태 타임라인, 업체 호출 기록 표(시도 · 결과 · 지연), trace 링크, 재시도 버튼 (`cancel_failed`면 "취소 재시도", Phase 6), 작업의 "보냄" 표시(Phase 6) |
| `/providers/:id` | 업체 상세 | p50/p95/p99 시계열 차트, 결과 분포, rate limit 여유 추이, 런타임 상태(AIMD로 줄인 허용 속도 포함), 정책 보기 · 편집(admin, Phase 6), 서킷 강제 조작, 지금 대사 |
| `/jobs` | 작업 큐 | 종류 · 상태별 개수, dead 작업 목록과 재큐잉 |
| `/reconciliation` | 대사 이슈 | 미해결 · 처리 완료 목록(업체 · 종류 필터), 처리 완료 표시, 업체별 "지금 대사", 이중 예약 의심 경고 (Phase 6). 메뉴에 미해결 수 배지 |
| `/chaos` | 장애 주입 | 업체별 프리셋 버튼, 세부 설정 슬라이더. 설정을 바꾸고 바로 대시보드에서 반응을 확인하는 용도 |
| `/audit` | 감사 로그 | 관리자가 무엇을 언제 했는지 (로그인 · 재처리 · 서킷 조작 · 정책 변경 · 장애 주입 · 대사), 조작 요청의 trace 링크 (Phase 6) |

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
   ├─ main.tsx               # 전역 설정: Mantine 테마, QueryClient(401이면 세션을 비움), BrowserRouter(basename /admin)
   ├─ App.tsx                # 로그인 여부로 갈림 → 레이아웃(머리글 + 왼쪽 메뉴) + 라우트. 화면은 lazy로 나눠 내려받는다
   ├─ api/
   │  ├─ schema.d.ts         # 자동 생성 (직접 수정 금지)
   │  ├─ client.ts           # openapi-fetch 클라이언트, unwrap(실패면 ApiError를 던짐)
   │  └─ queries/            # 화면별 TanStack Query 훅: bookings, jobs, providers, stats, chaos, info,
   │                         #   session(로그인 · 역할), reconciliation, audit (Phase 6)
   ├─ pages/                 # 경로별 화면: Dashboard, Bookings, BookingDetail, Provider, Jobs, Reconciliation,
   │                         #   Chaos, Audit, Login, NotFound
   ├─ components/            # 공용 부품: Badges(상태 · 결과 · 서킷), TraceLink, ProviderCharts, QueryState,
   │                         #   Permission(역할이 모자라면 버튼에 이유를 보여 줌)
   └─ lib/                   # 순수 함수: format(시각 · 지연 · 비율), labels(상태 · 이슈 종류 · 역할별 이름 · 색), grafana(링크)
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

## 6. 인증과 권한

처음 설계 (Phase 6 전):
- `admin_users` 테이블 + 비밀번호 해시(argon2)
- `POST /admin/api/login` → 세션 쿠키 (`HttpOnly`, `SameSite=Strict`, `Secure`), tower-sessions 사용
- 역할: `viewer`(조회만), `operator`(재처리 · 서킷 조작 · 장애 주입), `admin`(정책 변경)
- 관리자 조작은 모두 `booking_events.actor`나 별도 감사 로그에 남긴다

**구현 (Phase 6)**: tower-sessions의 PostgreSQL 저장소가 sqlx 0.8에 묶여 있어서 세션을 직접 만들었다 ([D-36](decisions.md#d-36-관리자-인증-argon2-비밀번호--db-세션-쿠키-권한은-추출기-타입으로-조작은-감사-로그로)).

```text
booking-server create-admin --username boss --role admin   비밀번호(36자)를 한 번만 보여 준다
POST /admin/api/login {username, password}
  └─ argon2로 확인 (spawn_blocking) → 256비트 토큰 발급, DB(admin_sessions)에는 SHA-256 해시만
     Set-Cookie: booking_admin_session=<토큰>; Path=/admin; HttpOnly; SameSite=Strict; Max-Age=43200; Secure
이후 관리자 API ── 쿠키의 토큰을 해시해 세션을 찾는다 → 역할 확인 (핸들러의 매개변수 타입)
POST /admin/api/logout ── DB의 세션을 지우고 쿠키를 지운다 (Max-Age=0)
```

| 역할 | 할 수 있는 일 | 서버 쪽 |
|---|---|---|
| `viewer` | 모든 조회 | `AdminSession` (로그인했으면 누구나) |
| `operator` | + 예약 재시도 · 작업 재큐잉 · 서킷 조작 · 장애 주입 · 지금 대사 · 대사 이슈 처리 완료 | `RequireOperator` |
| `admin` | + 업체 정책 변경 | `RequireAdmin` |

- 관리자 API는 로그인이 필요하다 (없으면 401, 역할이 모자라면 403). 예외: `POST /login`, `GET /info`(로그인 화면도 쓰는 화면 설정. 비밀이 아닌 값만).
- 관리자 조작은 `admin_audit_log`에 조작과 같은 트랜잭션으로 남는다 (`/audit` 화면). 예약 상태를 바꾼 조작은 상태 이력에 `admin:<이름>`으로도 남는다.
- 화면은 `GET /admin/api/session`으로 로그인 여부와 역할을 안다. 401이면 로그인 화면을 그린다. 어떤 요청이든 401을 받으면(세션 만료) `main.tsx`가 세션 정보를 비워 로그인 화면으로 돌아간다. 역할이 모자란 버튼은 꺼 두고, 그 위에 필요한 역할을 툴팁으로 보여 준다 (`NeedsRole`). 화면에서 끄는 것은 편의이고, 막는 것은 서버다.
- 개발 서버(Vite :5173)는 `/admin/api`를 서버로 프록시하므로 쿠키가 같은 주소(localhost)에 붙는다. `Secure` 쿠키는 https에서만 보내지만 브라우저가 localhost를 예외로 둔다. localhost가 아닌 http 주소로 쓰거나 localhost 예외가 없는 브라우저라면 `.env`에 `BOOKING_ADMIN_COOKIE_SECURE=false`.

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


## 8. 구현 메모 (Phase 6)

| 데이터 | 다시 읽는 간격 |
|---|---|
| 지금 세션 (`/admin/api/session`) | 1분 동안은 다시 묻지 않는다. 다른 요청이 401을 받으면 바로 비운다 |
| 미해결 대사 이슈 수 (메뉴 배지 · 대시보드) / 이슈 목록 | 5초 / 10초 |
| 감사 로그 | 10초 |

- **로그인 여부로 화면이 갈린다** (`App.tsx`): 세션을 읽는 중이면 로딩, 없으면(`null`) 로그인 화면, 있으면 메뉴와 화면. 로그인 · 로그아웃 훅은 응답으로 세션 값을 직접 바꿔(`setQueryData`) 다시 묻지 않고 화면이 바로 바뀐다.
- **인증 오류는 한곳에서**: `QueryCache` · `MutationCache`의 `onError`(≈ 전역 예외 필터)가 모든 요청의 401을 보고 세션을 비운다. 401 · 403은 다시 해도 같은 결과라 TanStack Query의 자동 재시도에서 뺐다 (다른 오류는 한 번만 다시 시도).
- **역할에 따라 버튼을 끈다**: `useCan('operator')`로 버튼을 끄고 `NeedsRole`이 툴팁으로 이유를 보여 준다. 꺼진 버튼은 마우스 이벤트를 받지 않아서 툴팁을 감싸는 요소에 단다.
- **정책 편집 폼** (업체 상세, admin): 지금 적용 중인 값으로 폼을 채우고, 바뀐 값이 있을 때만 "저장 (바로 반영)"이 켜진다. 서버가 거절하면(lease보다 긴 호출 등) Problem Details의 `detail`을 그대로 보여 준다. 저장하면 업체 상세 · 목록을 다시 읽는다.
- **대사 이슈 화면**: 필터(미해결 · 처리 완료, 업체, 종류)는 주소에 둔다 (`/reconciliation?resolved=true`). 이슈 내용(`detail`)은 한 줄 요약(우리 상태 → 업체 상태, 업체 예약 번호)과 툴팁의 JSON 원문으로 보여 준다. 이중 예약 의심이 있으면 화면 위에 빨간 경고와 할 일을 띄우고, 메뉴 배지도 빨간색이 된다.
- **포기한 대사 작업의 "다시 넣기"**는 그 행을 되살리지 않고 그 업체의 대사 작업(다른 ID)을 지금으로 당긴다. 알림이 그 작업 번호를 알려 준다 ([D-33](decisions.md#d-33-대사는-업체별-작업-체인으로-돌고-자동으로-바로잡을-것과-사람에게-넘길-것을-나눈다)).
- 확인한 것: 실제 서버 · mock 업체로 로그인(admin · viewer), viewer의 조작 버튼이 꺼지는 것, 대시보드의 이슈 카드, 대사 이슈 목록, 업체 상세의 AIMD 표시와 정책 편집 → 저장 → 감사 로그를 브라우저에서 끝까지 돌려 봤다.
