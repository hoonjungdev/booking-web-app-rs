// 대사(reconciliation) 이슈와 지금 대사 (docs/03-resilience.md §9)
import { notifications } from '@mantine/notifications'
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { api, errorMessage, type Schemas, unwrap } from '../client'

export type IssueFilters = {
  /** false: 미해결 (기본), true: 처리 완료 */
  resolved?: boolean
  provider?: string
  kind?: Schemas['IssueKindView']
}

/** 대사 이슈 목록 (커서 페이지네이션). 10초마다 다시 읽는다. */
export function useIssues(filters: IssueFilters) {
  return useInfiniteQuery({
    queryKey: ['reconciliation', 'issues', filters],
    queryFn: ({ pageParam }) =>
      unwrap(
        api.GET('/admin/api/reconciliation/issues', {
          params: { query: { ...filters, cursor: pageParam, limit: 50 } },
        }),
      ),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (lastPage) => lastPage.next_cursor ?? undefined,
    refetchInterval: 10_000,
  })
}

/** 미해결 이슈 수 (대시보드 · 메뉴). 5초마다 다시 읽는다. */
export function useIssueCounts() {
  return useQuery({
    queryKey: ['reconciliation', 'counts'],
    queryFn: () => unwrap(api.GET('/admin/api/reconciliation/counts')),
    refetchInterval: 5_000,
  })
}

/** 이슈를 처리 완료로 표시한다 */
export function useResolveIssue() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) =>
      unwrap(
        api.POST('/admin/api/reconciliation/issues/{id}/resolve', { params: { path: { id } } }),
      ),
    onSuccess: () => {
      notifications.show({ color: 'green', title: '대사 이슈', message: '처리 완료로 표시했습니다.' })
      return queryClient.invalidateQueries({ queryKey: ['reconciliation'] })
    },
    onError: (error) => {
      notifications.show({ color: 'red', title: '처리 실패', message: errorMessage(error) })
    },
  })
}

/** 업체의 대사를 지금 돌린다 (다음 정기 대사를 기다리지 않고) */
export function useRunReconcile() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (provider: string) =>
      unwrap(
        api.POST('/admin/api/providers/{id}/reconcile', { params: { path: { id: provider } } }),
      ),
    onSuccess: (job, provider) => {
      notifications.show({
        color: 'green',
        title: `${provider}: 대사`,
        message: `대사 작업 #${job.id}을(를) 곧 처리합니다. 찾은 불일치는 대사 이슈에 쌓입니다.`,
      })
      // 대사가 끝나면 예약 상태와 이슈가 바뀐다. 몇 초 뒤의 주기적 갱신이 반영하지만, 작업 목록은 바로 다시 읽는다.
      return queryClient.invalidateQueries({ queryKey: ['jobs'] })
    },
    onError: (error) => {
      notifications.show({ color: 'red', title: '대사 실패', message: errorMessage(error) })
    },
  })
}
