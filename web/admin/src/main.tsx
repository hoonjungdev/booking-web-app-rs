// 앱의 시작점: 전역 설정(테마, 서버 데이터 캐시, 라우터)을 깔고 App을 그린다.
// (≈ ASP.NET Core의 Program.cs에서 서비스를 등록하고 미들웨어를 조립하는 곳)
import '@mantine/core/styles.css'
import '@mantine/charts/styles.css'
import '@mantine/notifications/styles.css'

import { createTheme, MantineProvider } from '@mantine/core'
import { Notifications } from '@mantine/notifications'
import { MutationCache, QueryCache, QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter } from 'react-router'

import { App } from './App'
import { ApiError } from './api/client'
import { SESSION_KEY } from './api/queries/session'

const theme = createTheme({
  primaryColor: 'blue',
  defaultRadius: 'md',
})

/** 로그인이 필요하거나(401) 권한이 모자란(403) 오류: 다시 시도해도 같은 결과다 */
function isAuthError(error: unknown): boolean {
  return error instanceof ApiError && (error.status === 401 || error.status === 403)
}

/**
 * 어떤 요청이든 401(로그인 필요)을 받으면 세션이 끝난 것이다. 세션 정보를 비우면 App이 로그인 화면을 그린다.
 * (세션 조회 자신은 401을 `null`로 바꿔 돌려주므로 여기에 오지 않는다)
 */
function onAuthError(error: unknown) {
  if (error instanceof ApiError && error.status === 401) {
    queryClient.setQueryData(SESSION_KEY, null)
  }
}

/**
 * 서버 데이터 캐시. 화면들이 같은 데이터(queryKey)를 쓰면 한 번만 요청하고 결과를 나눠 쓴다.
 * - `retry`: 실패하면 한 번만 더 시도한다 (기본 3번은 관리자 화면에서 오류를 늦게 보여 준다). 인증 오류는 다시 하지 않는다
 * - `staleTime`: 받은 지 2초 안의 데이터는 새로 요청하지 않고 그대로 쓴다
 * - `queryCache` · `mutationCache`의 `onError`: 모든 요청의 실패를 한곳에서 본다 (≈ 전역 예외 필터)
 */
const queryClient = new QueryClient({
  queryCache: new QueryCache({ onError: onAuthError }),
  mutationCache: new MutationCache({ onError: onAuthError }),
  defaultOptions: {
    queries: {
      retry: (failureCount, error) => !isAuthError(error) && failureCount < 1,
      staleTime: 2_000,
    },
  },
})

const root = document.getElementById('root')
if (!root) {
  throw new Error('index.html에 #root 요소가 없습니다')
}

createRoot(root).render(
  <StrictMode>
    <MantineProvider theme={theme} defaultColorScheme="auto">
      <Notifications position="top-right" />
      <QueryClientProvider client={queryClient}>
        {/* 이 앱은 /admin 아래에서 돈다. 화면 안의 주소(/bookings)는 실제로 /admin/bookings가 된다. */}
        <BrowserRouter basename="/admin">
          <App />
        </BrowserRouter>
      </QueryClientProvider>
    </MantineProvider>
  </StrictMode>,
)
