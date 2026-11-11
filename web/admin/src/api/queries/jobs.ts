import { notifications } from '@mantine/notifications'
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { api, errorMessage, type Schemas, unwrap } from '../client'

export type JobFilters = {
  status?: Schemas['JobStatus']
  kind?: string
}

/** 작업 종류 · 상태별 개수. 5초마다 다시 읽는다. */
export function useJobCounts() {
  return useQuery({
    queryKey: ['jobs', 'counts'],
    queryFn: () => unwrap(api.GET('/admin/api/jobs/counts')),
    refetchInterval: 5_000,
  })
}

/** 작업 목록 (커서 페이지네이션) */
export function useJobList(filters: JobFilters) {
  return useInfiniteQuery({
    queryKey: ['jobs', 'list', filters],
    queryFn: ({ pageParam }) =>
      unwrap(
        api.GET('/admin/api/jobs', {
          params: { query: { ...filters, cursor: pageParam, limit: 50 } },
        }),
      ),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (lastPage) => lastPage.next_cursor ?? undefined,
    refetchInterval: 5_000,
  })
}

/** dead 작업을 다시 대기열에 넣는다 */
export function useRequeueJob() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) =>
      unwrap(api.POST('/admin/api/jobs/{id}/requeue', { params: { path: { id } } })),
    onSuccess: (result) => {
      notifications.show({
        color: 'green',
        title: '재처리',
        message: result.booking
          ? '작업을 다시 넣었습니다. 실패했던 예약은 업체에서 먼저 찾아본 뒤 처리합니다.'
          : '작업을 다시 넣었습니다.',
      })
      return queryClient.invalidateQueries()
    },
    onError: (error) => {
      notifications.show({ color: 'red', title: '재처리 실패', message: errorMessage(error) })
    },
  })
}
