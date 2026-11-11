// 업체 상세: 런타임 상태와 서킷 강제 조작, 호출 통계 차트(지연 분위수 · 결과 분포 · 남은 호출 수), 정책(편집)
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
  NumberInput,
  Progress,
  SegmentedControl,
  SimpleGrid,
  Stack,
  Switch,
  Table,
  Text,
  Title,
} from '@mantine/core'
import { IconEdit, IconLock, IconLockOpen, IconPlayerPlay, IconRotate } from '@tabler/icons-react'
import { useState } from 'react'
import { useParams } from 'react-router'

import type { Schemas } from '../api/client'
import { useAdminInfo } from '../api/queries/info'
import { useCircuitControl, useProvider, useUpdatePolicy } from '../api/queries/providers'
import { useRunReconcile } from '../api/queries/reconciliation'
import { useCan } from '../api/queries/session'
import { TIME_RANGES, type TimeRange, useProviderTimeseries } from '../api/queries/stats'
import { CircuitBadge } from '../components/Badges'
import { NeedsRole } from '../components/Permission'
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
        <Group gap="md">
          <ReconcileButton id={provider.id} />
          <GrafanaLink />
        </Group>
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
      <PolicyCard id={provider.id} policy={policy} restartRequired={restart_required} />
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
  const canOperate = useCan('operator')
  const { circuit } = runtime
  const breaker = policy.circuit_breaker

  return (
    <Card withBorder>
      <Group justify="space-between" mb="md">
        <Group gap="sm">
          <Title order={4}>서킷 브레이커</Title>
          <CircuitBadge circuit={circuit} />
        </Group>
        <NeedsRole role="operator">
          <Group gap="xs">
            <Button
              size="xs"
              color="red"
              variant="light"
              leftSection={<IconLock size={14} />}
              disabled={!canOperate}
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
              disabled={!canOperate}
              loading={control.isPending && control.variables === 'force_close'}
              onClick={() => control.mutate('force_close')}
            >
              강제 닫기
            </Button>
            <Button
              size="xs"
              variant="default"
              leftSection={<IconRotate size={14} />}
              disabled={!canOperate}
              loading={control.isPending && control.variables === 'reset'}
              onClick={() => control.mutate('reset')}
            >
              해제 (처음부터)
            </Button>
          </Group>
        </NeedsRole>
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
            {runtime.limiter_available} / {runtime.limiter_burst} · 허용 속도 초당 {runtime.rate_limit_allowed}
            {runtime.rate_limit_allowed < runtime.rate_limit_ceiling && (
              <Text span c="orange">
                {' '}
                (상한 {runtime.rate_limit_ceiling}에서 줄임: 업체가 한도 초과를 알림, AIMD)
              </Text>
            )}
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

/** 장애 대응 정책: 지금 적용 중인 값. admin은 편집해 바로 반영할 수 있다 (D-28, Phase 6). */
function PolicyCard({
  id,
  policy,
  restartRequired,
}: {
  id: string
  policy: Schemas['PolicyView']
  restartRequired: boolean
}) {
  const [editing, setEditing] = useState(false)
  const canEdit = useCan('admin')
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
      `초당 ${policy.rate_limit.per_second}회, 버킷 ${policy.rate_limit.burst}${policy.rate_limit.adaptive ? ', 적응형(AIMD)' : ''}`,
      `토큰을 최대 ${formatMs(policy.rate_limit.max_wait_ms)} 기다린다. 적응형이면 업체가 한도 초과를 알릴 때 속도를 절반으로 줄였다가 조금씩 되돌린다`,
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
    [
      '빈 시간 캐시',
      formatMs(policy.availability_cache_ttl_ms),
      '이 시간 동안은 캐시 값을 준다. 업체 호출이 실패하면 5분까지는 예전 값을 stale로 준다',
    ],
  ]
  return (
    <Card withBorder>
      <Group justify="space-between" mb="xs">
        <Title order={4}>장애 대응 정책</Title>
        {!editing && (
          <NeedsRole role="admin">
            <Button
              size="xs"
              variant="light"
              leftSection={<IconEdit size={14} />}
              disabled={!canEdit}
              onClick={() => setEditing(true)}
            >
              편집
            </Button>
          </NeedsRole>
        )}
      </Group>
      <Text size="sm" c="dimmed" mb="sm">
        지금 적용 중인 값이다. 편집해 저장하면 DB에 남기고 바로 반영한다 (다음 호출부터. 진행 중인 호출은 시작할 때의 정책으로
        끝난다).
      </Text>
      {restartRequired && (
        <Alert color="yellow" mb="sm" title="DB의 정책이 다릅니다">
          DB의 정책이 지금 적용 중인 값(아래)과 다릅니다. DB를 직접 고쳤거나 다른 서버 프로세스에서 바꾼 경우입니다. 이
          화면에서 저장하면 바로 반영되고, 서버를 다시 시작해도 DB 값이 반영됩니다.
        </Alert>
      )}
      {editing ? (
        // 서버 값이 바뀌면(저장 후) key가 바뀌어 폼을 새로 만든다 (입력 중인 값은 폼의 상태에만 둔다)
        <PolicyForm
          key={JSON.stringify(policy)}
          id={id}
          initial={policy}
          onDone={() => setEditing(false)}
        />
      ) : (
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
      )}
    </Card>
  )
}

type Policy = Schemas['PolicyView']

/** 정책 편집 폼. 입력하는 동안의 값은 이 컴포넌트의 상태(useState)에 두고, 저장할 때 서버로 보낸다. */
function PolicyForm({ id, initial, onDone }: { id: string; initial: Policy; onDone: () => void }) {
  const [draft, setDraft] = useState<Policy>(initial)
  const save = useUpdatePolicy(id)
  const changed = JSON.stringify(draft) !== JSON.stringify(initial)

  /** 숫자 입력칸 하나. `read`로 값을 꺼내고 `write`로 새 정책을 만든다 (객체를 통째로 새로 만들어야 React가 바뀐 것을 안다) */
  const field = (
    label: string,
    read: (p: Policy) => number,
    write: (p: Policy, value: number) => Policy,
    options: { min?: number; max?: number; step?: number; decimal?: boolean } = {},
  ) => (
    <NumberInput
      label={label}
      min={options.min ?? 0}
      max={options.max}
      step={options.step}
      allowDecimal={options.decimal ?? false}
      value={read(draft)}
      onChange={(value) => setDraft((current) => write(current, Number(value) || 0))}
    />
  )

  return (
    <Stack>
      <SimpleGrid cols={{ base: 1, sm: 2, lg: 3 }}>
        {field('시도 타임아웃 (ms)', (p) => p.attempt_timeout_ms, (p, v) => ({ ...p, attempt_timeout_ms: v }), { min: 1, max: 60_000 })}
        {field('호출 예산 (ms)', (p) => p.call_budget_ms, (p, v) => ({ ...p, call_budget_ms: v }), { min: 1 })}
        {field('빈 시간 캐시 (ms)', (p) => p.availability_cache_ttl_ms, (p, v) => ({ ...p, availability_cache_ttl_ms: v }))}
        {field('재시도: 최대 시도', (p) => p.retry.max_attempts, (p, v) => ({ ...p, retry: { ...p.retry, max_attempts: v } }), { min: 1 })}
        {field('재시도: 시작 간격 (ms)', (p) => p.retry.base_delay_ms, (p, v) => ({ ...p, retry: { ...p.retry, base_delay_ms: v } }))}
        {field('재시도: 최대 간격 (ms)', (p) => p.retry.max_delay_ms, (p, v) => ({ ...p, retry: { ...p.retry, max_delay_ms: v } }))}
        {field('rate limit: 초당', (p) => p.rate_limit.per_second, (p, v) => ({ ...p, rate_limit: { ...p.rate_limit, per_second: v } }), { min: 1 })}
        {field('rate limit: 버킷', (p) => p.rate_limit.burst, (p, v) => ({ ...p, rate_limit: { ...p.rate_limit, burst: v } }), { min: 1 })}
        {field('rate limit: 최대 대기 (ms)', (p) => p.rate_limit.max_wait_ms, (p, v) => ({ ...p, rate_limit: { ...p.rate_limit, max_wait_ms: v } }))}
        {field('bulkhead: 동시 실행', (p) => p.bulkhead.max_concurrency, (p, v) => ({ ...p, bulkhead: { ...p.bulkhead, max_concurrency: v } }), { min: 1 })}
        {field('bulkhead: 최대 대기 (ms)', (p) => p.bulkhead.max_wait_ms, (p, v) => ({ ...p, bulkhead: { ...p.bulkhead, max_wait_ms: v } }))}
        {field('서킷: 창 크기', (p) => p.circuit_breaker.window, (p, v) => ({ ...p, circuit_breaker: { ...p.circuit_breaker, window: v } }), { min: 1 })}
        {field('서킷: 최소 호출 수', (p) => p.circuit_breaker.min_calls, (p, v) => ({ ...p, circuit_breaker: { ...p.circuit_breaker, min_calls: v } }), { min: 1 })}
        {field('서킷: 실패율 (0~1)', (p) => p.circuit_breaker.failure_rate, (p, v) => ({ ...p, circuit_breaker: { ...p.circuit_breaker, failure_rate: v } }), { min: 0, max: 1, step: 0.05, decimal: true })}
        {field('서킷: 열림 유지 (ms)', (p) => p.circuit_breaker.open_ms, (p, v) => ({ ...p, circuit_breaker: { ...p.circuit_breaker, open_ms: v } }), { min: 1 })}
        {field('서킷: 시험 호출 수', (p) => p.circuit_breaker.half_open_probes, (p, v) => ({ ...p, circuit_breaker: { ...p.circuit_breaker, half_open_probes: v } }), { min: 1 })}
      </SimpleGrid>
      <Switch
        label="적응형 rate limit (AIMD): 업체가 한도 초과를 알리면 속도를 절반으로, 한동안 문제없으면 조금씩 되돌린다"
        checked={draft.rate_limit.adaptive}
        onChange={(event) => {
          const adaptive = event.currentTarget.checked
          setDraft((current) => ({ ...current, rate_limit: { ...current.rate_limit, adaptive } }))
        }}
      />
      <Group>
        <Button
          onClick={() => save.mutate(draft, { onSuccess: onDone })}
          loading={save.isPending}
          disabled={!changed}
        >
          저장 (바로 반영)
        </Button>
        <Button variant="subtle" onClick={onDone}>
          취소
        </Button>
      </Group>
    </Stack>
  )
}

/** "지금 대사" 버튼 (operator) */
function ReconcileButton({ id }: { id: string }) {
  const run = useRunReconcile()
  const canOperate = useCan('operator')
  return (
    <NeedsRole role="operator">
      <Button
        size="xs"
        variant="light"
        leftSection={<IconPlayerPlay size={14} />}
        disabled={!canOperate}
        loading={run.isPending}
        onClick={() => run.mutate(id)}
      >
        지금 대사
      </Button>
    </NeedsRole>
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
