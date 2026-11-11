import { notifications } from '@mantine/notifications'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { CHAOS_PRESET } from '../../lib/labels'
import { api, errorMessage, type Schemas, unwrap } from '../client'

/** mock 업체의 지금 장애 설정 */
export function useChaos(provider: string, enabled: boolean) {
  return useQuery({
    queryKey: ['chaos', provider],
    queryFn: () =>
      unwrap(api.GET('/admin/api/chaos/{provider}', { params: { path: { provider } } })),
    // 장애 주입이 꺼져 있으면(BOOKING_CHAOS_URL 없음) 요청하지 않는다
    enabled,
  })
}

/** 미리 정해 둔 장애 시나리오 적용 */
export function useApplyPreset(provider: string) {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (name: Schemas['ChaosPreset']) =>
      unwrap(
        api.POST('/admin/api/chaos/{provider}/preset/{name}', {
          params: { path: { provider, name } },
        }),
      ),
    onSuccess: (settings, name) => {
      // 응답에 바뀐 설정이 들어 있으므로 다시 읽지 않고 그대로 넣어 둔다
      queryClient.setQueryData(['chaos', provider], settings)
      notifications.show({
        color: name === 'healthy' ? 'green' : 'orange',
        title: `${provider}: ${CHAOS_PRESET[name].label}`,
        message: CHAOS_PRESET[name].description,
      })
    },
    onError: (error) => {
      notifications.show({ color: 'red', title: '장애 주입 실패', message: errorMessage(error) })
    },
  })
}

/** 세부 장애 설정 저장 (전체 교체) */
export function useSaveChaos(provider: string) {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (settings: Schemas['ChaosSettings']) =>
      unwrap(
        api.PUT('/admin/api/chaos/{provider}', {
          params: { path: { provider } },
          body: settings,
        }),
      ),
    onSuccess: (settings) => {
      queryClient.setQueryData(['chaos', provider], settings)
      notifications.show({ color: 'green', title: provider, message: '장애 설정을 저장했습니다.' })
    },
    onError: (error) => {
      notifications.show({ color: 'red', title: '저장 실패', message: errorMessage(error) })
    },
  })
}
