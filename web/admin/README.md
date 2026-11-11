# 관리자 웹

booking-server의 관리자 화면 (React + TypeScript + Vite + Mantine + TanStack Query). 설계는 [docs/06-admin-frontend.md](../../docs/06-admin-frontend.md).

```bash
pnpm install        # 처음 한 번
pnpm dev            # http://localhost:5173/admin/  (booking-server :8080을 함께 띄워 둔다. /admin/api는 서버로 넘어간다)
pnpm build          # dist/ → booking-server가 http://127.0.0.1:8080/admin/ 에서 제공
pnpm typecheck      # 타입 검사
pnpm lint           # ESLint
pnpm gen            # 서버 API를 바꾼 뒤: openapi.json(서버 코드에서) → src/api/schema.d.ts(타입) 다시 만들기
```

- `openapi.json`과 `src/api/schema.d.ts`는 자동 생성 파일이다. 직접 고치지 않고 `pnpm gen`으로 만들어 함께 커밋한다. CI(`scripts/check.sh api` · `web`)가 코드와 같은지 검사한다.
- 서버 데이터는 `src/api/queries/`의 훅으로만 가져온다 (`useEffect` 안에서 fetch하지 않는다).
- 의존성은 정확한 버전으로 고정하고, 공개된 지 1주 이상 지난 버전을 고른다 ([D-29](../../docs/decisions.md#d-29-관리자-웹-의존성은-정확한-버전으로-고정하고-공개된-지-1주-이상-지난-버전을-쓴다-typescript는-5x)).
