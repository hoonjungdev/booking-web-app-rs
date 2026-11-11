// 업체 상세의 차트들 (@mantine/charts는 recharts를 Mantine 스타일로 감싼 것이다)
import { BarChart, LineChart } from '@mantine/charts'
import { Text } from '@mantine/core'

import type { Schemas } from '../api/client'
import { formatMs, formatShortTime } from '../lib/format'
import { OUTCOME } from '../lib/labels'

type Points = Schemas['TimeseriesPoint'][]

const CHART_HEIGHT = 220

/** p50 / p95 / p99 지연 (실제로 업체를 부른 시도만) */
export function LatencyChart({ points }: { points: Points }) {
  const data = points.map((p) => ({
    time: formatShortTime(p.bucket),
    p50: p.latency_ms.p50 ?? null,
    p95: p.latency_ms.p95 ?? null,
    p99: p.latency_ms.p99 ?? null,
  }))
  return (
    <LineChart
      h={CHART_HEIGHT}
      data={data}
      dataKey="time"
      series={[
        { name: 'p50', color: 'teal.6' },
        { name: 'p95', color: 'orange.6' },
        { name: 'p99', color: 'red.6' },
      ]}
      curveType="monotone"
      withDots={false}
      // 호출이 없던 구간은 선을 끊지 않고 잇는다
      connectNulls
      valueFormatter={(value) => formatMs(value)}
      withLegend
    />
  )
}

/** 결과별 시도 수 (쌓은 막대). 데이터에 나온 결과만 그린다. */
export function OutcomeChart({ points }: { points: Points }) {
  const seen = new Set(points.flatMap((p) => Object.keys(p.by_outcome)))
  // 미리 정해 둔 순서(성공 → 실패 → 막힘)대로, 처음 보는 결과는 뒤에 붙인다
  const order = [...Object.keys(OUTCOME), ...[...seen].filter((o) => !(o in OUTCOME))]
  const outcomes = order.filter((o) => seen.has(o))
  if (outcomes.length === 0) {
    return (
      <Text c="dimmed" ta="center" py="xl">
        이 기간에는 호출이 없습니다
      </Text>
    )
  }
  const data = points.map((p) => ({ time: formatShortTime(p.bucket), ...p.by_outcome }))
  return (
    <BarChart
      h={CHART_HEIGHT}
      data={data}
      dataKey="time"
      type="stacked"
      series={outcomes.map((o) => ({
        name: o,
        label: OUTCOME[o]?.label ?? o,
        color: `${OUTCOME[o]?.color ?? 'gray'}.6`,
      }))}
      withLegend
    />
  )
}

/** 업체가 알려 준 남은 호출 수 (구간마다 가장 작은 값) */
export function RateLimitChart({ points }: { points: Points }) {
  const data = points.map((p) => ({
    time: formatShortTime(p.bucket),
    remaining: p.min_rate_limit_remaining ?? null,
  }))
  if (data.every((d) => d.remaining === null)) {
    return (
      <Text c="dimmed" ta="center" py="xl">
        업체가 남은 호출 수를 알려 준 응답이 없습니다
      </Text>
    )
  }
  return (
    <LineChart
      h={CHART_HEIGHT}
      data={data}
      dataKey="time"
      series={[{ name: 'remaining', label: '남은 호출 수 (최소)', color: 'grape.6' }]}
      curveType="stepAfter"
      withDots={false}
      connectNulls
    />
  )
}
