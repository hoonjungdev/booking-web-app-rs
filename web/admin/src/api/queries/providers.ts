import { notifications } from '@mantine/notifications'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { api, errorMessage, type Schemas, unwrap } from '../client'

/** 업체 목록 + 런타임 요약. `refetchInterval`: 이 간격(ms)마다 다시 읽는다 (대시보드는 5초). */
export function useProviders(refetchInterval?: number) {
  return useQuery({
    queryKey: ['providers'],
    queryFn: () => unwrap(api.GET('/admin/api/providers')),
    refetchInterval,
  })
}

/** 업체 상세 (정책 + 런타임 상태). 서킷 상태가 바뀌는 것을 보도록 3초마다 다시 읽는다. */
export function useProvider(id: string) {
  return useQuery({
    queryKey: ['providers', id],
    queryFn: () =>
      unwrap(api.GET('/admin/api/providers/{id}', { params: { path: { id } } })),
    refetchInterval: 3_000,
  })
}

type CircuitAction = Schemas['CircuitActionView']

const ACTION_DONE: Record<CircuitAction, string> = {
  force_open: '서킷을 강제로 열었습니다. 해제할 때까지 업체를 부르지 않습니다.',
  force_close: '서킷을 강제로 닫았습니다. 해제할 때까지 실패가 쌓여도 열리지 않습니다.',
  reset: '서킷의 강제 상태를 풀고 처음부터 다시 셉니다.',
}

/**
 * 서킷 강제 조작 (`useMutation`: 서버의 상태를 바꾸는 요청).
 * 성공하면 업체 정보를 다시 읽게 한다(`invalidateQueries`). 그래야 화면에 바뀐 상태가 바로 보인다.
 */
export function useCircuitControl(id: string) {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (action: CircuitAction) =>
      unwrap(
        api.POST('/admin/api/providers/{id}/circuit', {
          params: { path: { id } },
          body: { action },
        }),
      ),
    onSuccess: (_runtime, action) => {
      notifications.show({ color: 'green', title: '서킷 조작', message: ACTION_DONE[action] })
      return queryClient.invalidateQueries({ queryKey: ['providers'] })
    },
    onError: (error) => {
      notifications.show({ color: 'red', title: '서킷 조작 실패', message: errorMessage(error) })
    },
  })
}
