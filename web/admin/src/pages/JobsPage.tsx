// 작업 큐: 종류 · 상태별 개수, 작업 목록과 dead 작업 재큐잉
import {
  Anchor,
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
import { IconRefresh } from '@tabler/icons-react'
import { Link, useSearchParams } from 'react-router'

import { type JobFilters, useJobCounts, useJobList, useRequeueJob } from '../api/queries/jobs'
import { JobStatusBadge } from '../components/Badges'
import { ErrorAlert, Loading } from '../components/QueryState'
import { formatCount, formatDateTime, formatRelative, shortId } from '../lib/format'
import { JOB_STATUS, JOB_STATUSES, type JobStatus } from '../lib/labels'

export default function JobsPage() {
  const [params, setParams] = useSearchParams()
  // 처음 열면 관리자가 볼 일이 있는 dead 작업부터 보여 준다
  const status = (params.get('status') ?? 'dead') as JobStatus | 'all'
  const kind = params.get('kind') ?? undefined
  const filters: JobFilters = { status: status === 'all' ? undefined : status, kind }

  const counts = useJobCounts()
  const list = useJobList(filters)
  const requeue = useRequeueJob()
  const jobs = list.data?.pages.flatMap((page) => page.items) ?? []

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
      <Title order={2}>작업 큐</Title>

      <Card withBorder>
        <Title order={4} mb="sm">
          종류별 개수
        </Title>
        {counts.isError && <ErrorAlert error={counts.error} />}
        {counts.data && (
          <Table>
            <Table.Thead>
              <Table.Tr>
                <Table.Th>종류</Table.Th>
                <Table.Th>대기</Table.Th>
                <Table.Th>미룸</Table.Th>
                <Table.Th>실행 중</Table.Th>
                <Table.Th>dead</Table.Th>
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {counts.data.kinds.map(({ kind, counts: c }) => (
                <Table.Tr key={kind}>
                  <Table.Td>
                    <Code>{kind}</Code>
                  </Table.Td>
                  <Table.Td>{formatCount(c.ready)}</Table.Td>
                  <Table.Td>{formatCount(c.scheduled)}</Table.Td>
                  <Table.Td>{formatCount(c.running)}</Table.Td>
                  <Table.Td c={c.dead > 0 ? 'red' : undefined} fw={c.dead > 0 ? 700 : undefined}>
                    {formatCount(c.dead)}
                  </Table.Td>
                </Table.Tr>
              ))}
            </Table.Tbody>
          </Table>
        )}
        <Text size="xs" c="dimmed" mt="xs">
          대기: 지금 실행할 수 있음 · 미룸: 재시도나 서킷 열림 때문에 나중에 실행 · dead: 더 시도하지 않고 묻어 둠
          (관리자 확인 대상). 끝난 작업은 세지 않는다.
        </Text>
      </Card>

      <Group align="flex-end">
        <Stack gap={4}>
          <Text size="sm" fw={500}>
            상태
          </Text>
          <SegmentedControl
            data={[
              { value: 'all', label: '전체' },
              ...JOB_STATUSES.map((s) => ({ value: s, label: JOB_STATUS[s].label })),
            ]}
            value={status}
            onChange={(value) => setFilter('status', value)}
          />
        </Stack>
        <Select
          label="종류"
          placeholder="전체"
          clearable
          data={counts.data?.kinds.map((k) => k.kind) ?? []}
          value={kind ?? null}
          onChange={(value) => setFilter('kind', value)}
          w={220}
        />
      </Group>

      {list.isPending && <Loading />}
      {list.isError && <ErrorAlert error={list.error} />}
      {list.data && jobs.length === 0 && (
        <Text c="dimmed" py="lg">
          조건에 맞는 작업이 없습니다
        </Text>
      )}
      {jobs.length > 0 && (
        <Table.ScrollContainer minWidth={900}>
          <Table striped highlightOnHover>
            <Table.Thead>
              <Table.Tr>
                <Table.Th>ID</Table.Th>
                <Table.Th>종류</Table.Th>
                <Table.Th>예약</Table.Th>
                <Table.Th>상태</Table.Th>
                <Table.Th>시도</Table.Th>
                <Table.Th>실행 시각</Table.Th>
                <Table.Th>마지막 오류</Table.Th>
                <Table.Th />
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {jobs.map((job) => (
                <Table.Tr key={job.id}>
                  <Table.Td>{job.id}</Table.Td>
                  <Table.Td>
                    <Code>{job.kind}</Code>
                  </Table.Td>
                  <Table.Td>
                    {job.booking_id ? (
                      <Anchor component={Link} to={`/bookings/${job.booking_id}`} ff="monospace">
                        {shortId(job.booking_id)}
                      </Anchor>
                    ) : (
                      '–'
                    )}
                  </Table.Td>
                  <Table.Td>
                    <JobStatusBadge status={job.status} />
                  </Table.Td>
                  <Table.Td>
                    {job.attempts} / {job.max_attempts}
                  </Table.Td>
                  <Table.Td>
                    <Tooltip label={formatDateTime(job.run_at)}>
                      <span>{formatRelative(job.run_at)}</span>
                    </Tooltip>
                  </Table.Td>
                  <Table.Td>
                    <Tooltip label={job.last_error} disabled={!job.last_error} multiline maw={480}>
                      <Text size="sm" lineClamp={1} maw={320}>
                        {job.last_error ?? ''}
                      </Text>
                    </Tooltip>
                  </Table.Td>
                  <Table.Td>
                    {job.status === 'dead' && (
                      <Button
                        size="compact-sm"
                        variant="light"
                        color="orange"
                        leftSection={<IconRefresh size={14} />}
                        loading={requeue.isPending && requeue.variables === job.id}
                        onClick={() => requeue.mutate(job.id)}
                      >
                        다시 넣기
                      </Button>
                    )}
                  </Table.Td>
                </Table.Tr>
              ))}
            </Table.Tbody>
          </Table>
        </Table.ScrollContainer>
      )}

      {list.hasNextPage && (
        <Button
          variant="light"
          onClick={() => list.fetchNextPage()}
          loading={list.isFetchingNextPage}
          w="fit-content"
        >
          더 보기
        </Button>
      )}
    </Stack>
  )
}
