// ESLint 설정 (flat config). `pnpm lint`로 실행한다.
import js from '@eslint/js'
import reactHooks from 'eslint-plugin-react-hooks'
import { reactRefresh } from 'eslint-plugin-react-refresh'
import { defineConfig, globalIgnores } from 'eslint/config'
import globals from 'globals'
import tseslint from 'typescript-eslint'

export default defineConfig([
  // 빌드 결과와 자동 생성 파일은 검사하지 않는다
  globalIgnores(['dist', 'src/api/schema.d.ts']),
  {
    files: ['**/*.{ts,tsx}'],
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      // 훅 규칙: 훅은 컴포넌트 최상단에서만 부르고, 의존성 배열을 빠뜨리지 않는다
      reactHooks.configs.flat['recommended-latest'],
      // 개발 서버의 빠른 새로 고침(HMR)이 동작하도록 파일마다 컴포넌트만 내보낸다
      reactRefresh.configs.vite(),
    ],
    languageOptions: {
      ecmaVersion: 2022,
      globals: globals.browser,
    },
  },
])
