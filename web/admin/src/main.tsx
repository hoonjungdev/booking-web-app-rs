// 앱의 시작점: 전역 설정(테마, 서버 데이터 캐시, 라우터)을 깔고 App을 그린다.
// (≈ ASP.NET Core의 Program.cs에서 서비스를 등록하고 미들웨어를 조립하는 곳)
import '@mantine/core/styles.css'
import '@mantine/charts/styles.css'
import '@mantine/notifications/styles.css'

import { createTheme, MantineProvider } from '@mantine/core'
import { Notifications } from '@mantine/notifications'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter } from 'react-router'

import { App } from './App'

const theme = createTheme({
  primaryColor: 'blue',
  defaultRadius: 'md',
})

/**
 * 서버 데이터 캐시. 화면들이 같은 데이터(queryKey)를 쓰면 한 번만 요청하고 결과를 나눠 쓴다.
 * - `retry: 1`: 실패하면 한 번만 더 시도한다 (기본 3번은 관리자 화면에서 오류를 늦게 보여 준다)
 * - `staleTime`: 받은 지 2초 안의 데이터는 새로 요청하지 않고 그대로 쓴다
 */
const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 1,
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
