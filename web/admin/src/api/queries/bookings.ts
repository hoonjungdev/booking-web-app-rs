import { notifications } from '@mantine/notifications'
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { api, errorMessage, type Schemas, unwrap } from '../client'

export type BookingFilters = {
  status?: Schemas['BookingStatus']
  provider?: string
  /** 접수 시각이 이 시각 이후 (RFC 3339) */
  from?: string
}

const PAGE_SIZE = 50

/**
 * 예약 목록. 커서 페이지네이션이라 `useInfiniteQuery`를 쓴다: 페이지를 차례로 이어 붙이고,
 * 서버가 준 `next_cursor`로 다음 페이지를 읽는다 (`fetchNextPage`). 조건이 바뀌면 queryKey가 바뀌어 처음부터 다시 읽는다.
 */
export function useBookingList(filters: BookingFilters) {
  return useInfiniteQuery({
    queryKey: ['bookings', 'list', filters],
    queryFn: ({ pageParam }) =>
      unwrap(
        api.GET('/admin/api/bookings', {
          params: { query: { ...filters, cursor: pageParam, limit: PAGE_SIZE } },
        }),
      ),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (lastPage) => lastPage.next_cursor ?? undefined,
    refetchInterval: 10_000,
  })
}

/** 아직 결론이 나지 않은 상태. 이 상태인 동안은 자주 다시 읽어 변화를 보여 준다. */
const IN_PROGRESS: Schemas['BookingStatus'][] = ['pending', 'unknown', 'cancel_requested']

/** 예약 상세 (상태 이력, 업체 호출 기록, 작업) */
export function useBooking(id: string) {
  return useQuery({
    queryKey: ['bookings', 'detail', id],
    queryFn: () =>
      unwrap(api.GET('/admin/api/bookings/{id}', { params: { path: { id } } })),
    // 처리 중이면 2초, 끝났으면 10초마다
    refetchInterval: (query) =>
      query.state.data && IN_PROGRESS.includes(query.state.data.booking.status) ? 2_000 : 10_000,
  })
}

/** 실패한 예약 재시도 */
export function useRetryBooking() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: string) =>
      unwrap(api.POST('/admin/api/bookings/{id}/retry', { params: { path: { id } } })),
    onSuccess: () => {
      notifications.show({
        color: 'green',
        title: '재시도',
        message: '예약을 다시 처리합니다. 워커가 업체에서 먼저 찾아본 뒤 없으면 다시 보냅니다.',
      })
      // 예약 · 작업 화면이 모두 바뀌므로 읽어 둔 데이터를 전부 다시 읽게 한다
      return queryClient.invalidateQueries()
    },
    onError: (error) => {
      notifications.show({ color: 'red', title: '재시도 실패', message: errorMessage(error) })
    },
  })
}
