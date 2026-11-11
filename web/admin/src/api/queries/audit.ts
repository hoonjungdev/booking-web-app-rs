// 감사 로그 (관리자 조작 기록)
import { useInfiniteQuery } from '@tanstack/react-query'

import { api, unwrap } from '../client'

/** 감사 로그 (최근 것부터, 커서 페이지네이션). 10초마다 다시 읽는다. */
export function useAuditLog() {
  return useInfiniteQuery({
    queryKey: ['audit'],
    queryFn: ({ pageParam }) =>
      unwrap(api.GET('/admin/api/audit', { params: { query: { cursor: pageParam, limit: 50 } } })),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (lastPage) => lastPage.next_cursor ?? undefined,
    refetchInterval: 10_000,
  })
}
