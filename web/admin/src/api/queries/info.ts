import { useQuery } from '@tanstack/react-query'

import { api, unwrap } from '../client'

/**
 * 관리자 화면 설정 (Grafana 주소, 장애 주입 · 업체 런타임 상태를 쓸 수 있는가).
 * 서버를 다시 띄우기 전에는 바뀌지 않으므로 한 번만 읽는다 (`staleTime: Infinity`).
 */
export function useAdminInfo() {
  return useQuery({
    queryKey: ['info'],
    queryFn: () => unwrap(api.GET('/admin/api/info')),
    staleTime: Infinity,
  })
}
