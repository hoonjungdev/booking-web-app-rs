// 대사 이슈: 업체 예약 목록과 우리 예약을 비교해 찾은 불일치 (docs/03-resilience.md §9)
//
// 대사는 업체마다 10분 간격으로 돈다. 업체측 취소처럼 자동으로 바로잡은 것은 "처리 완료"로 쌓이고,
// 이중 예약 의심 · 업체에 없음처럼 사람이 업체와 확인해야 하는 것은 "미해결"로 남는다.
import {
  Alert,
  Anchor,
  Badge,
  Button,
  Card,
  Code,
  Group,
  SegmentedControl,
  Select,
  Stack,
  Table,
  Text,
  Title,
  Tooltip,
} from '@mantine/core'
import { IconCheck, IconPlayerPlay } from '@tabler/icons-react'
import { Link, useSearchParams } from 'react-router'

import type { Schemas } from '../api/client'
import { useProviders } from '../api/queries/providers'
import {
  type IssueFilters,
  useIssueCounts,
  useIssues,
  useResolveIssue,
  useRunReconcile,
} from '../api/queries/reconciliation'
import { useCan } from '../api/queries/session'
import { NeedsRole } from '../components/Permission'
import { ErrorAlert, Loading } from '../components/QueryState'
import { formatDateTime, formatRelative, shortId } from '../lib/format'
import { ISSUE_KIND, ISSUE_KINDS, type IssueKind } from '../lib/labels'

export default function ReconciliationPage() {
  const [params, setParams] = useSearchParams()
  const resolved = params.get('resolved') === 'true'
  const filters: IssueFilters = {
    resolved,
    provider: params.get('provider') ?? undefined,
    kind: (params.get('kind') as IssueKind | null) ?? undefined,
  }
  const issues = useIssues(filters)
  const counts = useIssueCounts()
  const providers = useProviders()
  const items = issues.data?.pages.flatMap((page) => page.items) ?? []
  const duplicates = counts.data?.by_kind['duplicate_suspected'] ?? 0

  const setFilter = (key: string, value: string | null) => {
    const next = new URLSearchParams(params)
    if (value) {
      next.set(key, value)
    } else {
      next.delete(key)
    }
    setParams(next)
  }

  return (
    <Stack gap="lg">
      <Title order={2}>대사 이슈</Title>
      <Text c="dimmed" size="sm">
        대사는 업체마다 정기적으로(기본 10분) 지난 24시간 ~ 앞으로 7일 슬롯의 업체 예약 목록과 우리 예약을 비교합니다.
        업체측 취소처럼 자동으로 바로잡은 불일치는 처리 완료로, 업체와 확인해야 하는 불일치는 미해결로 남습니다.
      </Text>

      {duplicates > 0 && (
        <Alert color="red" title={`이중 예약 의심 ${duplicates}건`}>
          같은 예약이 업체에 두 번 있습니다. 업체에서 남길 예약 하나를 정해 나머지를 취소한 뒤 처리 완료로 표시하세요.
        </Alert>
      )}

      <ReconcileNow providers={providers.data ?? []} />

      <Group align="flex-end">
        <Stack gap={4}>
          <Text size="sm" fw={500}>
            상태
          </Text>
          <SegmentedControl
            data={[
              { value: 'false', label: `미해결${counts.data ? ` (${counts.data.open})` : ''}` },
              { value: 'true', label: '처리 완료' },
            ]}
            value={String(resolved)}
            onChange={(value) => setFilter('resolved', value === 'true' ? 'true' : null)}
          />
        </Stack>
        <Select
          label="업체"
          placeholder="전체"
          clearable
          data={providers.data?.map((p) => ({ value: p.id, label: p.display_name })) ?? []}
          value={filters.provider ?? null}
          onChange={(value) => setFilter('provider', value)}
          w={200}
        />
        <Select
          label="종류"
          placeholder="전체"
          clearable
          data={ISSUE_KINDS.map((kind) => ({ value: kind, label: ISSUE_KIND[kind].label }))}
          value={filters.kind ?? null}
          onChange={(value) => setFilter('kind', value)}
          w={200}
        />
      </Group>

      {issues.isPending && <Loading />}
      {issues.isError && <ErrorAlert error={issues.error} />}
      {issues.data && items.length === 0 && (
        <Text c="dimmed" py="lg">
          {resolved ? '처리 완료된 이슈가 없습니다' : '미해결 이슈가 없습니다'}
        </Text>
      )}
      {items.length > 0 && <IssueTable issues={items} />}

      {issues.hasNextPage && (
        <Button
          variant="light"
          onClick={() => issues.fetchNextPage()}
          loading={issues.isFetchingNextPage}
          w="fit-content"
        >
          더 보기
        </Button>
      )}
    </Stack>
  )
}

/** 업체마다 "지금 대사" 버튼 (operator) */
function ReconcileNow({ providers }: { providers: Schemas['ProviderView'][] }) {
  const run = useRunReconcile()
  const canOperate = useCan('operator')
  return (
    <Card withBorder>
      <Group justify="space-between">
        <Text size="sm">정기 대사를 기다리지 않고 지금 돌립니다 (업체 호출 한도에 여유가 있을 때만 업체를 부릅니다).</Text>
        <Group gap="xs">
          {providers.map((provider) => (
            <NeedsRole key={provider.id} role="operator">
              <Button
                size="xs"
                variant="light"
                leftSection={<IconPlayerPlay size={14} />}
                disabled={!canOperate}
                loading={run.isPending && run.variables === provider.id}
                onClick={() => run.mutate(provider.id)}
              >
                {provider.display_name} 대사
              </Button>
            </NeedsRole>
          ))}
        </Group>
      </Group>
    </Card>
  )
}

function IssueTable({ issues }: { issues: Schemas['IssueView'][] }) {
  const resolve = useResolveIssue()
  const canOperate = useCan('operator')
  return (
    <Table.ScrollContainer minWidth={960}>
      <Table striped highlightOnHover>
        <Table.Thead>
          <Table.Tr>
            <Table.Th>ID</Table.Th>
            <Table.Th>업체</Table.Th>
            <Table.Th>종류</Table.Th>
            <Table.Th>예약</Table.Th>
            <Table.Th>업체 예약</Table.Th>
            <Table.Th>내용</Table.Th>
            <Table.Th>찾은 시각</Table.Th>
            <Table.Th>처리</Table.Th>
          </Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {issues.map((issue) => {
            const kind = ISSUE_KIND[issue.kind]
            return (
              <Table.Tr key={issue.id}>
                <Table.Td>{issue.id}</Table.Td>
                <Table.Td>
                  <Anchor component={Link} to={`/providers/${issue.provider}`}>
                    {issue.provider}
                  </Anchor>
                </Table.Td>
                <Table.Td>
                  <Tooltip label={kind.description} multiline maw={320}>
                    <Badge color={kind.color} variant={issue.kind === 'duplicate_suspected' ? 'filled' : 'light'}>
                      {kind.label}
                    </Badge>
                  </Tooltip>
                </Table.Td>
                <Table.Td>
                  {issue.booking_id ? (
                    <Anchor component={Link} to={`/bookings/${issue.booking_id}`} ff="monospace">
                      {shortId(issue.booking_id)}
                    </Anchor>
                  ) : (
                    '–'
                  )}
                </Table.Td>
                <Table.Td>{issue.provider_booking_id ? <Code>{issue.provider_booking_id}</Code> : '–'}</Table.Td>
                <Table.Td>
                  <DetailSummary detail={issue.detail} />
                </Table.Td>
                <Table.Td>
                  <Tooltip label={formatDateTime(issue.detected_at)}>
                    <span>{formatRelative(issue.detected_at)}</span>
                  </Tooltip>
                </Table.Td>
                <Table.Td>
                  {issue.resolved_at ? (
                    <Tooltip label={formatDateTime(issue.resolved_at)}>
                      <Text size="sm" c="dimmed">
                        {issue.resolved_by === 'reconciler' ? '자동' : issue.resolved_by}
                      </Text>
                    </Tooltip>
                  ) : (
                    <NeedsRole role="operator">
                      <Button
                        size="compact-sm"
                        variant="light"
                        color="green"
                        leftSection={<IconCheck size={14} />}
                        disabled={!canOperate}
                        loading={resolve.isPending && resolve.variables === issue.id}
                        onClick={() => resolve.mutate(issue.id)}
                      >
                        처리 완료
                      </Button>
                    </NeedsRole>
                  )}
                </Table.Td>
              </Table.Tr>
            )
          })}
        </Table.Tbody>
      </Table>
    </Table.ScrollContainer>
  )
}

/** 이슈 내용 한 줄 요약 (우리 상태 → 업체 상태, 메모). 전체는 툴팁의 JSON으로 본다. */
function DetailSummary({ detail }: { detail: Record<string, unknown> }) {
  const ours = typeof detail.ours === 'string' ? detail.ours : '없음'
  const theirs = typeof detail.theirs === 'string' ? detail.theirs : '없음'
  const ids = Array.isArray(detail.provider_booking_ids) ? detail.provider_booking_ids.join(', ') : null
  const note = typeof detail.note === 'string' ? detail.note : null
  return (
    <Tooltip label={<pre style={{ margin: 0 }}>{JSON.stringify(detail, null, 2)}</pre>} multiline>
      <Text size="sm" lineClamp={2} maw={360}>
        {ids ? `업체 예약 ${ids}` : `우리 ${ours} · 업체 ${theirs}`}
        {note && ` — ${note}`}
      </Text>
    </Tooltip>
  )
}
