// 대시보드: 업체별 상태 카드, 작업 큐, 미해결 대사 이슈, 최근 실패한 예약. 5초마다 다시 읽는다.
//
// 장애를 주입하면 이 화면에서 서킷이 열리고(빨강) 성공률이 떨어지는 것을 보고,
// 복구한 뒤 dead 작업을 재처리하는 흐름의 출발점이다 (docs/07-roadmap.md Phase 5 완료 기준).
import {
  Alert,
  Anchor,
  Badge,
  Card,
  Group,
  Progress,
  SegmentedControl,
  SimpleGrid,
  Stack,
  Table,
  Text,
  Title,
  Tooltip,
} from '@mantine/core'
import { useState } from 'react'
import { Link } from 'react-router'

import type { Schemas } from '../api/client'
import { useBookingList } from '../api/queries/bookings'
import { useJobCounts } from '../api/queries/jobs'
import { useProviders } from '../api/queries/providers'
import { useIssueCounts } from '../api/queries/reconciliation'
import { type SummaryWindow, useStatsSummary } from '../api/queries/stats'
import { BookingStatusBadge, CircuitBadge } from '../components/Badges'
import { ErrorAlert, Loading } from '../components/QueryState'
import {
  formatCount,
  formatDateTime,
  formatMs,
  formatPercent,
  formatRelative,
  shortId,
} from '../lib/format'
import { ISSUE_KIND, ISSUE_KINDS } from '../lib/labels'

const WINDOWS: { value: SummaryWindow; label: string }[] = [
  { value: '5m', label: '5분' },
  { value: '15m', label: '15분' },
  { value: '1h', label: '1시간' },
]

export default function DashboardPage() {
  // 화면 안에서만 쓰는 상태는 useState로 둔다 (서버 데이터는 TanStack Query가 맡는다)
  const [period, setPeriod] = useState<SummaryWindow>('15m')
  const providers = useProviders(5_000)
  const summary = useStatsSummary(period)

  return (
    <Stack gap="lg">
      <Group justify="space-between">
        <Title order={2}>대시보드</Title>
        <Group gap="xs">
          <Text size="sm" c="dimmed">
            호출 통계 기간
          </Text>
          <SegmentedControl
            size="xs"
            data={WINDOWS}
            value={period}
            onChange={(value) => setPeriod(value as SummaryWindow)}
          />
        </Group>
      </Group>

      {providers.isPending && <Loading />}
      {providers.isError && <ErrorAlert error={providers.error} />}
      {providers.data && (
        <SimpleGrid cols={{ base: 1, md: 2, xl: 3 }}>
          {providers.data.map((provider) => (
            <ProviderCard
              key={provider.id}
              provider={provider}
              stats={summary.data?.providers.find((s) => s.provider === provider.id)}
            />
          ))}
        </SimpleGrid>
      )}

      <SimpleGrid cols={{ base: 1, lg: 3 }}>
        <JobQueueCard />
        <IssuesCard />
        <FailedBookingsCard />
      </SimpleGrid>
    </Stack>
  )
}

/** 업체 하나: 서킷 상태, 성공률, p95, 호출 제한, pause, 동시 실행 수 */
function ProviderCard({
  provider,
  stats,
}: {
  provider: Schemas['ProviderView']
  stats?: Schemas['ProviderStats']
}) {
  const runtime = provider.runtime
  return (
    <Card withBorder padding="md">
      <Group justify="space-between" mb="sm">
        <Anchor component={Link} to={`/providers/${provider.id}`} fw={600}>
          {provider.display_name}
        </Anchor>
        {runtime ? (
          <CircuitBadge circuit={runtime.circuit} />
        ) : (
          <Badge color="gray">{provider.enabled ? '런타임 정보 없음' : '사용 안 함'}</Badge>
        )}
      </Group>

      <SimpleGrid cols={3} spacing="xs">
        <Metric
          label="성공률"
          value={formatPercent(stats?.success_rate)}
          hint="업체가 정상으로 답한 비율 (업무적 거절 포함) ÷ 실제로 부른 시도"
          alert={stats?.success_rate != null && stats.success_rate < 0.95}
        />
        <Metric label="p95 지연" value={formatMs(stats?.latency_ms.p95)} hint="실제로 부른 시도만" />
        <Metric
          label="호출 / 막힘"
          value={`${formatCount(stats?.called ?? 0)} / ${formatCount(stats?.blocked ?? 0)}`}
          hint="실제로 부른 시도 / 서킷 · rate limiter에서 막혀 부르지 않은 시도"
        />
        <Metric
          label="429 (호출 제한)"
          value={formatCount(stats?.rate_limited ?? 0)}
          alert={(stats?.rate_limited ?? 0) > 0}
        />
        <Metric label="재시도" value={formatCount(stats?.retries ?? 0)} />
        <Metric
          label="pause"
          value={runtime?.paused_until ? formatRelative(runtime.paused_until) + '까지' : '아님'}
          hint="업체가 Retry-After로 쉬라고 해서 호출을 멈춘 상태"
          alert={Boolean(runtime?.paused_until)}
        />
      </SimpleGrid>

      {runtime && (
        <Stack gap={4} mt="sm">
          <Text size="xs" c="dimmed">
            동시 실행 {runtime.in_flight} / {runtime.max_concurrency}
          </Text>
          <Progress value={(runtime.in_flight / runtime.max_concurrency) * 100} size="sm" />
          <Text size="xs" c="dimmed">
            우리 limiter 토큰 {runtime.limiter_available} / {runtime.limiter_burst} · 초당 {runtime.rate_limit_allowed}
            {runtime.rate_limit_allowed < runtime.rate_limit_ceiling && ` (상한 ${runtime.rate_limit_ceiling}에서 줄임)`}
            {runtime.rate_limit_remaining != null && ` · 업체가 알려 준 남은 호출 ${runtime.rate_limit_remaining}`}
          </Text>
          <Progress
            value={(runtime.limiter_available / runtime.limiter_burst) * 100}
            size="sm"
            color="grape"
          />
        </Stack>
      )}
    </Card>
  )
}

function Metric({
  label,
  value,
  hint,
  alert = false,
}: {
  label: string
  value: string
  hint?: string
  alert?: boolean
}) {
  const body = (
    <div>
      <Text size="xs" c="dimmed">
        {label}
      </Text>
      <Text fw={600} c={alert ? 'red' : undefined}>
        {value}
      </Text>
    </div>
  )
  return hint ? <Tooltip label={hint}>{body}</Tooltip> : body
}

/** 작업 큐: 대기 · 미룸 · 실행 중 · dead */
function JobQueueCard() {
  const counts = useJobCounts()
  return (
    <Card withBorder>
      <Group justify="space-between" mb="sm">
        <Title order={4}>작업 큐</Title>
        <Anchor component={Link} to="/jobs" size="sm">
          작업 큐 보기
        </Anchor>
      </Group>
      {counts.isError && <ErrorAlert error={counts.error} />}
      {counts.data && (
        <>
          <SimpleGrid cols={4}>
            <Metric label="대기" value={formatCount(counts.data.total.ready)} />
            <Metric label="미룸" value={formatCount(counts.data.total.scheduled)} hint="재시도 · 서킷 열림으로 나중에 실행" />
            <Metric label="실행 중" value={formatCount(counts.data.total.running)} />
            <Metric label="dead" value={formatCount(counts.data.total.dead)} alert={counts.data.total.dead > 0} />
          </SimpleGrid>
          {counts.data.total.dead > 0 && (
            <Alert color="red" mt="sm" title="dead 작업이 있습니다">
              업체 장애가 끝났다면 <Anchor component={Link} to="/jobs?status=dead">작업 큐</Anchor>에서 다시
              넣으세요. 실패한 예약은 업체에서 먼저 찾아본 뒤 처리합니다.
            </Alert>
          )}
        </>
      )}
    </Card>
  )
}

/** 미해결 대사 이슈: 종류별 개수. 이중 예약 의심이 있으면 빨갛게 알린다. */
function IssuesCard() {
  const counts = useIssueCounts()
  const duplicates = counts.data?.by_kind['duplicate_suspected'] ?? 0
  return (
    <Card withBorder>
      <Group justify="space-between" mb="sm">
        <Title order={4}>대사 이슈</Title>
        <Anchor component={Link} to="/reconciliation" size="sm">
          대사 이슈 보기
        </Anchor>
      </Group>
      {counts.isError && <ErrorAlert error={counts.error} />}
      {counts.data && (
        <>
          <SimpleGrid cols={2}>
            {ISSUE_KINDS.map((kind) => (
              <Metric
                key={kind}
                label={ISSUE_KIND[kind].label}
                value={formatCount(counts.data.by_kind[kind] ?? 0)}
                hint={ISSUE_KIND[kind].description}
                alert={(counts.data.by_kind[kind] ?? 0) > 0}
              />
            ))}
          </SimpleGrid>
          {duplicates > 0 && (
            <Alert color="red" mt="sm" title="이중 예약 의심">
              업체에 같은 예약이 두 번 있습니다. <Anchor component={Link} to="/reconciliation?kind=duplicate_suspected">대사 이슈</Anchor>에서
              확인하세요.
            </Alert>
          )}
          {counts.data.open === 0 && (
            <Text size="sm" c="dimmed" mt="sm">
              미해결 이슈가 없습니다
            </Text>
          )}
        </>
      )}
    </Card>
  )
}

/** 최근 실패한 예약 5건 */
function FailedBookingsCard() {
  const failed = useBookingList({ status: 'failed' })
  const items = failed.data?.pages[0]?.items.slice(0, 5) ?? []
  return (
    <Card withBorder>
      <Group justify="space-between" mb="sm">
        <Title order={4}>최근 실패한 예약</Title>
        <Anchor component={Link} to="/bookings?status=failed" size="sm">
          모두 보기
        </Anchor>
      </Group>
      {failed.isError && <ErrorAlert error={failed.error} />}
      {failed.data && items.length === 0 && <Text c="dimmed">실패한 예약이 없습니다</Text>}
      {items.length > 0 && (
        <Table>
          <Table.Tbody>
            {items.map((booking) => (
              <Table.Tr key={booking.id}>
                <Table.Td>
                  <Anchor component={Link} to={`/bookings/${booking.id}`} ff="monospace">
                    {shortId(booking.id)}
                  </Anchor>
                </Table.Td>
                <Table.Td>
                  <BookingStatusBadge status={booking.status} />
                </Table.Td>
                <Table.Td>
                  <Text size="sm" lineClamp={1}>
                    {booking.status_reason ?? '–'}
                  </Text>
                </Table.Td>
                <Table.Td>
                  <Text size="xs" c="dimmed">
                    {formatDateTime(booking.updated_at)}
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
