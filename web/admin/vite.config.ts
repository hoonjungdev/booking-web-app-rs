import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// https://vite.dev/config/
export default defineConfig({
  // booking-server가 이 앱을 /admin 아래에서 내려준다 (docs/06-admin-frontend.md §5).
  // 빌드한 파일 안의 경로(/admin/assets/…)가 이 값을 기준으로 만들어진다.
  base: '/admin/',
  plugins: [react()],
  server: {
    port: 5173,
    // 개발 중에는 Vite(:5173)가 화면을, booking-server(:8080)가 API를 맡는다.
    // 화면이 같은 주소의 /admin/api를 부르면 Vite가 서버로 넘겨 준다 (CORS 설정이 필요 없다).
    proxy: {
      '/admin/api': 'http://127.0.0.1:8080',
    },
  },
})
