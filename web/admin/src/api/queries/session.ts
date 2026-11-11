// 관리자 로그인 세션과 역할 (docs/06-admin-frontend.md §6)
//
// 서버가 로그인할 때 세션 쿠키(HttpOnly)를 준다. 화면의 JavaScript는 그 쿠키를 읽을 수 없고, 브라우저가 같은 주소의
// 요청에 알아서 실어 보낸다. 그래서 화면은 "지금 로그인했나, 누구인가"를 `/admin/api/session`에 물어 안다.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { api, ApiError, type Schemas, unwrap } from '../client'

export type Session = Schemas['SessionView']
export type AdminRole = Schemas['AdminRole']

/** 세션 데이터의 queryKey. main.tsx의 전역 오류 처리도 이 키를 쓴다. */
export const SESSION_KEY = ['session'] as const

/** 지금 로그인한 관리자. 로그인하지 않았거나 세션이 끝났으면(401) `null` */
export function useSession() {
  return useQuery({
    queryKey: SESSION_KEY,
    queryFn: async (): Promise<Session | null> => {
      try {
        return await unwrap(api.GET('/admin/api/session'))
      } catch (error) {
        if (error instanceof ApiError && error.status === 401) {
          return null
        }
        throw error
      }
    },
    // 세션은 자주 바뀌지 않는다. 다른 요청이 401을 받으면 main.tsx가 이 값을 null로 바꾼다.
    staleTime: 60_000,
  })
}

/** 역할의 높낮이: viewer(조회) < operator(재처리 · 서킷 · 장애 주입 · 대사) < admin(정책 변경) */
const RANK: Record<AdminRole, number> = { viewer: 0, operator: 1, admin: 2 }

/** 지금 관리자가 `role` 이상의 권한을 가졌는가. 화면은 이것으로 버튼을 끈다 (실제 검사는 서버가 한다). */
export function useCan(role: AdminRole): boolean {
  const session = useSession()
  const current = session.data?.role
  return current != null && RANK[current] >= RANK[role]
}

export function useLogin() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (body: { username: string; password: string }) =>
      unwrap(api.POST('/admin/api/login', { body })),
    onSuccess: (session) => {
      // 응답에 세션이 들어 있으므로 다시 묻지 않고 넣어 둔다. 그러면 App이 바로 관리자 화면을 그린다.
      queryClient.setQueryData(SESSION_KEY, session)
    },
  })
}

export function useLogout() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: () => unwrap(api.POST('/admin/api/logout')),
    onSuccess: () => {
      // 읽어 둔 데이터를 모두 버린다 (다음 사람이 앞 사람의 화면을 보지 않게)
      queryClient.clear()
      queryClient.setQueryData(SESSION_KEY, null)
    },
  })
}
