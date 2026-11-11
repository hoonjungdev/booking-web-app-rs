// 업체 상세: 런타임 상태와 서킷 강제 조작, 호출 통계 차트(지연 분위수 · 결과 분포 · 남은 호출 수), 정책
//
// 차트의 원천은 호출 기록(provider_calls)이다. Grafana의 p95는 히스토그램 버킷으로 보간한 근사값이고,
// 여기의 p95는 기록 한 줄 한 줄로 계산한 정확한 값이다 (docs/05-observability-and-analytics.md §2).
import {
  Alert,
  Anchor,
  Badge,
  Button,
  Card,
  Code,
  Group,
  Progress,
  SegmentedControl,
  SimpleGrid,
  Stack,
  Table,
  Text,
  Title,
} from '@mantine/core'
import { IconLock, IconLockOpen, IconRotate } from '@tabler/icons-react'
import { useState } from 'react'
import { useParams } from 'react-router'

import type { Schemas } from '../api/client'
import { useAdminInfo } from '../api/queries/info'
import { useCircuitControl, useProvider } from '../api/queries/providers'
import { TIME_RANGES, type TimeRange, useProviderTimeseries } from '../api/queries/stats'
import { CircuitBadge } from '../components/Badges'
import { LatencyChart, OutcomeChart, RateLimitChart } from '../components/ProviderCharts'
import { ErrorAlert, Loading } from '../components/QueryState'
import { formatDateTime, formatMs, formatPercent, formatRelative } from '../lib/format'
import { dashboardUrl } from '../lib/grafana'

export default function ProviderPage() {
  const { id = '' } = useParams()
  const detail = useProvider(id)

  if (detail.isPending) return <Loading />
  if (detail.isError) return <ErrorAlert error={detail.error} />

  const { provider, policy, restart_required } = detail.data
  return (
    <Stack gap="lg">
      <Group justify="space-between">
        <Stack gap={2}>
          <Group gap="sm">
            <Title order={2}>{provider.display_name}</Title>
            <Badge color={provider.enabled ? 'green' : 'gray'}>
              {provider.enabled ? '사용 중' : '사용 안 함'}
            </Badge>
          </Group>
          <Text size="sm" c="dimmed">
            <Code>{provider.id}</Code> · {provider.adapter_kind} · {provider.base_url}
          </Text>
        </Stack>
        <GrafanaLink />
      </Group>

      {provider.runtime ? (
        <RuntimeCard id={provider.id} runtime={provider.runtime} policy={policy} />
      ) : (
        <Alert color="gray" title="런타임 상태를 볼 수 없습니다">
          이 서버 프로세스에는 업체 파이프라인이 없습니다. 서킷 상태는 업체를 부르는 프로세스(워커)의 메모리에 있으므로,
          서버를 <Code>--role all</Code>(기본값)로 실행해야 여기서 보고 조작할 수 있습니다.
        </Alert>
      )}

      <StatsCharts id={provider.id} />
      <PolicyCard policy={policy} restartRequired={restart_required} />
    </Stack>
  )
}

/** 서킷 · pause · bulkhead · rate limiter의 지금 상태와 서킷 강제 조작 */
function RuntimeCard({
  id,
  runtime,
  policy,
}: {
  id: string
  runtime: Schemas['RuntimeView']
  policy: Schemas['PolicyView']
}) {
  const control = useCircuitControl(id)
  const { circuit } = runtime
  const breaker = policy.circuit_breaker

  return (
    <Card withBorder>
      <Group justify="space-between" mb="md">
        <Group gap="sm">
          <Title order={4}>서킷 브레이커</Title>
          <CircuitBadge circuit={circuit} />
        </Group>
        <Group gap="xs">
          <Button
            size="xs"
            color="red"
            variant="light"
            leftSection={<IconLock size={14} />}
            loading={control.isPending && control.variables === 'force_open'}
            onClick={() => control.mutate('force_open')}
          >
            강제 열기
          </Button>
          <Button
            size="xs"
            color="green"
            variant="light"
            leftSection={<IconLockOpen size={14} />}
            loading={control.isPending && control.variables === 'force_close'}
            onClick={() => control.mutate('force_close')}
          >
            강제 닫기
          </Button>
          <Button
            size="xs"
            variant="default"
            leftSection={<IconRotate size={14} />}
            loading={control.isPending && control.variables === 'reset'}
            onClick={() => control.mutate('reset')}
          >
            해제 (처음부터)
          </Button>
        </Group>
      </Group>

      <Text size="sm" mb="md">
        <CircuitExplanation circuit={circuit} breaker={breaker} />
      </Text>

      <SimpleGrid cols={{ base: 1, sm: 3 }}>
        <Stack gap={4}>
          <Text size="sm" fw={500}>
            동시 실행 (bulkhead)
          </Text>
          <Progress value={(runtime.in_flight / runtime.max_concurrency) * 100} />
          <Text size="xs" c="dimmed">
            {runtime.in_flight} / {runtime.max_concurrency}
          </Text>
        </Stack>
        <Stack gap={4}>
          <Text size="sm" fw={500}>
            우리 rate limiter 토큰
          </Text>
          <Progress color="grape" value={(runtime.limiter_available / runtime.limiter_burst) * 100} />
          <Text size="xs" c="dimmed">
            {runtime.limiter_available} / {runtime.limiter_burst} (초당 {policy.rate_limit.per_second}개씩 다시 참)
          </Text>
        </Stack>
        <Stack gap={4}>
          <Text size="sm" fw={500}>
            업체 신호
          </Text>
          <Text size="sm">
            {runtime.paused_until ? (
              <Text span c="red">
                pause 중 ({formatRelative(runtime.paused_until)}까지)
              </Text>
            ) : (
              'pause 아님'
            )}
          </Text>
          <Text size="xs" c="dimmed">
            업체가 알려 준 남은 호출 수: {runtime.rate_limit_remaining ?? '아직 없음'}
          </Text>
        </Stack>
      </SimpleGrid>
    </Card>
  )
}

/** 서킷이 지금 어떤 판단을 하고 있는지 한 문장으로 */
function CircuitExplanation({
  circuit,
  breaker,
}: {
  circuit: Schemas['CircuitView']
  breaker: Schemas['CircuitBreakerPolicyView']
}) {
  if (circuit.forced === 'open') {
    return <>관리자가 강제로 열었습니다. 해제할 때까지 업체를 부르지 않고, 작업은 시도 횟수를 쓰지 않고 미룹니다.</>
  }
  if (circuit.forced === 'closed') {
    return <>관리자가 강제로 닫았습니다. 해제할 때까지 실패가 쌓여도 열리지 않습니다.</>
  }
  switch (circuit.state) {
    case 'closed': {
      const rate = circuit.recent_calls > 0 ? circuit.recent_failures / circuit.recent_calls : null
      return (
        <>
          최근 {circuit.recent_calls}회 중 {circuit.recent_failures}회 실패 ({formatPercent(rate)}). 최근{' '}
          {breaker.window}회 중 {breaker.min_calls}회 이상 호출했고 실패율이 {formatPercent(breaker.failure_rate)} 이상이면
          열립니다.
        </>
      )
    }
    case 'open':
      return (
        <>
          업체를 부르지 않고 있습니다.{' '}
          {circuit.open_until &&
            `${formatDateTime(circuit.open_until)}(${formatRelative(circuit.open_until)}) 이후 다음 호출이 오면 시험 호출을 시작합니다.`}
        </>
      )
    case 'half_open':
      return (
        <>
          시험 호출 중: {breaker.half_open_probes}회 중 {circuit.probes_succeeded}회 성공, {circuit.probes_in_flight}회
          진행 중. 모두 성공하면 닫히고 하나라도 실패하면 다시 열립니다.
        </>
      )
  }
}

/** 호출 기록으로 그린 차트 3개 */
function StatsCharts({ id }: { id: string }) {
  const [range, setRange] = useState<TimeRange>('1h')
  const series = useProviderTimeseries(id, range)
  const points = series.data?.points ?? []
  const totals = points.reduce(
    (sum, p) => ({ calls: sum.calls + p.calls, retries: sum.retries + p.retries }),
    { calls: 0, retries: 0 },
  )

  return (
    <Stack>
      <Group justify="space-between">
        <Title order={3}>호출 통계</Title>
        <SegmentedControl
          size="xs"
          data={Object.entries(TIME_RANGES).map(([value, { label }]) => ({ value, label }))}
          value={range}
          onChange={(value) => setRange(value as TimeRange)}
        />
      </Group>
      {series.isError && <ErrorAlert error={series.error} />}
      {series.isPending && <Loading />}
      {series.data && (
        <>
          <Text size="sm" c="dimmed">
            {formatDateTime(series.data.from)} ~ 지금 · {series.data.interval} 간격 · 시도 {totals.calls}회 (재시도{' '}
            {totals.retries}회)
          </Text>
          <SimpleGrid cols={{ base: 1, xl: 2 }}>
            <Card withBorder>
              <Text fw={500} mb="sm">
                지연 p50 / p95 / p99
              </Text>
              <LatencyChart points={points} />
            </Card>
            <Card withBorder>
              <Text fw={500} mb="sm">
                결과 분포
              </Text>
              <OutcomeChart points={points} />
            </Card>
            <Card withBorder>
              <Text fw={500} mb="sm">
                업체가 알려 준 남은 호출 수 (rate limit 여유)
              </Text>
              <RateLimitChart points={points} />
            </Card>
          </SimpleGrid>
        </>
      )}
    </Stack>
  )
}

/** 장애 대응 정책 (읽기 전용). 지금 실제로 적용 중인 값이다. */
function PolicyCard({
  policy,
  restartRequired,
}: {
  policy: Schemas['PolicyView']
  restartRequired: boolean
}) {
  const rows: [string, string, string][] = [
    ['시도 타임아웃', formatMs(policy.attempt_timeout_ms), '시도 한 번의 최대 시간'],
    ['호출 예산', formatMs(policy.call_budget_ms), '호출 한 번(재시도 포함)의 최대 시간'],
    [
      '호출 내 재시도',
      `${policy.retry.max_attempts}회, ${formatMs(policy.retry.base_delay_ms)} → 최대 ${formatMs(policy.retry.max_delay_ms)}`,
      '지수 백오프 + full jitter',
    ],
    [
      'rate limit',
      `초당 ${policy.rate_limit.per_second}회, 버킷 ${policy.rate_limit.burst}`,
      `토큰을 최대 ${formatMs(policy.rate_limit.max_wait_ms)} 기다린다`,
    ],
    [
      'bulkhead',
      `동시 ${policy.bulkhead.max_concurrency}개`,
      `자리를 최대 ${formatMs(policy.bulkhead.max_wait_ms)} 기다린다`,
    ],
    [
      '서킷 브레이커',
      `최근 ${policy.circuit_breaker.window}회 중 실패율 ${formatPercent(policy.circuit_breaker.failure_rate)} (최소 ${policy.circuit_breaker.min_calls}회)`,
      `열리면 ${formatMs(policy.circuit_breaker.open_ms)} 뒤 시험 호출 ${policy.circuit_breaker.half_open_probes}회`,
    ],
  ]
  return (
    <Card withBorder>
      <Title order={4} mb="xs">
        장애 대응 정책
      </Title>
      <Text size="sm" c="dimmed" mb="sm">
        서버가 시작할 때 DB(<Code>providers.policy</Code>)에서 읽어 적용한 값이다. 화면에서 바꾸고 바로 반영하는 기능은
        Phase 6(정책 실시간 변경)에서 추가한다.
      </Text>
      {restartRequired && (
        <Alert color="yellow" mb="sm" title="DB의 정책이 바뀌었습니다">
          서버가 시작한 뒤 DB의 정책이 바뀌어 지금 적용 중인 값(아래)과 다릅니다. 서버를 다시 시작하면 반영됩니다.
        </Alert>
      )}
      <Table>
        <Table.Tbody>
          {rows.map(([name, value, note]) => (
            <Table.Tr key={name}>
              <Table.Td fw={500}>{name}</Table.Td>
              <Table.Td>{value}</Table.Td>
              <Table.Td>
                <Text size="sm" c="dimmed">
                  {note}
                </Text>
              </Table.Td>
            </Table.Tr>
          ))}
        </Table.Tbody>
      </Table>
    </Card>
  )
}

function GrafanaLink() {
  const info = useAdminInfo()
  const grafana = info.data?.grafana_url
  if (!grafana) return null
  return (
    <Anchor href={dashboardUrl(grafana, 'booking-provider-overview')} target="_blank" rel="noreferrer" size="sm">
      Grafana: Provider Overview
    </Anchor>
  )
}
