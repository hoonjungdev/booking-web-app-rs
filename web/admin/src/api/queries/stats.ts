import { useQuery } from '@tanstack/react-query'

import { api, type Schemas, unwrap } from '../client'

export type SummaryWindow = Schemas['SummaryWindow']

/** 업체별 호출 요약 (대시보드 카드). 5초마다 다시 읽는다. */
export function useStatsSummary(window: SummaryWindow) {
  return useQuery({
    queryKey: ['stats', 'summary', window],
    queryFn: () =>
      unwrap(api.GET('/admin/api/stats/providers/summary', { params: { query: { window } } })),
    refetchInterval: 5_000,
  })
}

/** 업체 상세 차트의 기간. 기간마다 차트의 점이 60~100개쯤 되도록 구간 간격을 고른다. */
export const TIME_RANGES = {
  '15m': { label: '15분', minutes: 15, interval: '10s' },
  '1h': { label: '1시간', minutes: 60, interval: '1m' },
  '6h': { label: '6시간', minutes: 360, interval: '5m' },
  '24h': { label: '24시간', minutes: 1440, interval: '15m' },
} as const satisfies Record<
  string,
  { label: string; minutes: number; interval: Schemas['StatsInterval'] }
>

export type TimeRange = keyof typeof TIME_RANGES

/**
 * 업체 하나의 구간별 호출 통계. 10초마다 "지금부터 거슬러 올라간 기간"을 다시 읽는다.
 * 시작 · 끝 시각은 읽을 때마다 새로 계산하므로 queryKey에는 넣지 않는다 (넣으면 매번 다른 데이터로 취급된다).
 */
export function useProviderTimeseries(id: string, range: TimeRange) {
  return useQuery({
    queryKey: ['stats', 'timeseries', id, range],
    queryFn: () => {
      const { minutes, interval } = TIME_RANGES[range]
      const to = new Date()
      const from = new Date(to.getTime() - minutes * 60_000)
      return unwrap(
        api.GET('/admin/api/stats/providers/{id}/timeseries', {
          params: {
            path: { id },
            query: { from: from.toISOString(), to: to.toISOString(), interval },
          },
        }),
      )
    },
    refetchInterval: 10_000,
    // 기간을 바꾸는 동안 이전 차트를 그대로 보여 준다 (빈 화면이 깜박이지 않게)
    placeholderData: (previous) => previous,
  })
}
