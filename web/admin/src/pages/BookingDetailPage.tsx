// 예약 상세: 상태 타임라인, 업체 호출 기록, 작업, trace 링크, 재시도 버튼
import {
  Alert,
  Anchor,
  Badge,
  Button,
  Card,
  Code,
  CopyButton,
  Group,
  SimpleGrid,
  Stack,
  Table,
  Text,
  Timeline,
  Title,
  Tooltip,
} from '@mantine/core'
import { IconArrowLeft, IconRefresh } from '@tabler/icons-react'
import type { ReactNode } from 'react'
import { Link, useParams } from 'react-router'

import type { Schemas } from '../api/client'
import { useBooking, useRetryBooking } from '../api/queries/bookings'
import { useRequeueJob } from '../api/queries/jobs'
import { useCan } from '../api/queries/session'
import { BookingStatusBadge, JobStatusBadge, OutcomeBadge } from '../components/Badges'
import { NeedsRole } from '../components/Permission'
import { ErrorAlert, Loading } from '../components/QueryState'
import { TraceLink } from '../components/TraceLink'
import { formatDateTime, formatMs, formatRelative, formatTime } from '../lib/format'
import { BOOKING_STATUS, outcomeLabel } from '../lib/labels'

export default function BookingDetailPage() {
  // 경로의 `:id` 부분 (App.tsx의 `/bookings/:id`)
  const { id = '' } = useParams()
  const detail = useBooking(id)

  return (
    <Stack>
      <Anchor component={Link} to="/bookings" size="sm">
        <IconArrowLeft size={14} /> 예약 목록
      </Anchor>
      {detail.isPending && <Loading />}
      {detail.isError && <ErrorAlert error={detail.error} />}
      {detail.data && <BookingDetail detail={detail.data} />}
    </Stack>
  )
}

function BookingDetail({ detail }: { detail: Schemas['BookingDetail'] }) {
  const { booking, events, calls, jobs } = detail
  const retry = useRetryBooking()
  const canOperate = useCan('operator')
  // 실패한 예약은 다시 제출하고, 취소에 실패한 예약은 취소를 다시 보낸다
  const retryable = booking.status === 'failed' || booking.status === 'cancel_failed'

  return (
    <Stack gap="lg">
      <Group justify="space-between" align="flex-start">
        <Stack gap={4}>
          <Group gap="sm">
            <Title order={2}>예약</Title>
            <BookingStatusBadge status={booking.status} />
          </Group>
          <Group gap={4}>
            <Code>{booking.id}</Code>
            <CopyButton value={booking.id}>
              {({ copied, copy }) => (
                <Button variant="subtle" size="compact-xs" onClick={copy}>
                  {copied ? '복사함' : '복사'}
                </Button>
              )}
            </CopyButton>
          </Group>
        </Stack>
        {retryable && (
          <NeedsRole role="operator">
            <Button
              leftSection={<IconRefresh size={16} />}
              color="orange"
              disabled={!canOperate}
              loading={retry.isPending}
              onClick={() => retry.mutate(booking.id)}
            >
              {booking.status === 'cancel_failed' ? '취소 재시도' : '재시도'}
            </Button>
          </NeedsRole>
        )}
      </Group>

      {booking.status === 'cancel_failed' && (
        <Alert color="orange" title="취소에 실패한 예약">
          {booking.status_reason ?? '사유 없음'}
          <Text size="sm" mt={4}>
            업체에서 취소하지 못했습니다. 업체와 확인한 뒤 &quot;취소 재시도&quot;를 누르면 취소를 다시 보냅니다 (이미 취소된
            예약이어도 결과가 같습니다).
          </Text>
        </Alert>
      )}

      {booking.status === 'failed' && (
        <Alert color="orange" title="실패한 예약">
          {booking.status_reason ?? '사유 없음'}
          <Text size="sm" mt={4}>
            재시도하면 예약을 처리 중으로 되돌리고, 워커가 업체에서 이 예약을 먼저 찾아봅니다. 이미 만들어져 있으면
            다시 보내지 않고 확정으로 끝냅니다 (이중 예약 방지).
          </Text>
        </Alert>
      )}

      <Card withBorder>
        <SimpleGrid cols={{ base: 1, sm: 2, lg: 3 }} spacing="md">
          <Field label="업체">
            <Anchor component={Link} to={`/providers/${booking.provider}`}>
              {booking.provider}
            </Anchor>
          </Field>
          <Field label="자원">{booking.resource_id}</Field>
          <Field label="슬롯 시작">{formatDateTime(booking.slot_start)}</Field>
          <Field label="인원">{booking.party_size}명</Field>
          <Field label="업체 예약 ID">{booking.provider_booking_id ?? '–'}</Field>
          <Field label="사유">{booking.status_reason ?? '–'}</Field>
          <Field label="확정 기한">
            {formatDateTime(booking.deadline_at)} ({formatRelative(booking.deadline_at)})
          </Field>
          <Field label="접수">{formatDateTime(booking.created_at)}</Field>
          <Field label="마지막 변경">{formatDateTime(booking.updated_at)}</Field>
          <Field label="클라이언트">
            <Code>{booking.client_id}</Code>
          </Field>
        </SimpleGrid>
      </Card>

      <SimpleGrid cols={{ base: 1, lg: 2 }}>
        <Card withBorder>
          <Title order={4} mb="md">
            상태 이력
          </Title>
          <EventTimeline events={events} />
        </Card>
        <Card withBorder>
          <Title order={4} mb="md">
            작업
          </Title>
          <JobTable jobs={jobs} />
        </Card>
      </SimpleGrid>

      <Card withBorder>
        <Title order={4} mb="xs">
          업체 호출 기록
        </Title>
        <Text size="sm" c="dimmed" mb="md">
          시도 한 번 = 한 줄. 같은 호출 안의 재시도는 시도 번호가 늘어난다. 서킷 · rate limiter에서 막힌 시도는
          업체를 부르지 않았으므로 지연이 0에 가깝다.
        </Text>
        <CallTable calls={calls} />
      </Card>
    </Stack>
  )
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <Text size="xs" c="dimmed">
        {label}
      </Text>
      <Text size="sm" component="div">
        {children}
      </Text>
    </div>
  )
}

/** 상태가 바뀐 순서. 누가(api · worker · admin) 왜 바꿨는지, 어느 trace에서 일어났는지 보여 준다. */
function EventTimeline({ events }: { events: Schemas['BookingEventView'][] }) {
  return (
    <Timeline active={events.length - 1} bulletSize={18} lineWidth={2}>
      {events.map((event, index) => (
        <Timeline.Item
          key={index}
          color={BOOKING_STATUS[event.to_status].color}
          title={
            <Group gap="xs">
              {event.from_status ? (
                <>
                  <Text size="sm">{BOOKING_STATUS[event.from_status].label}</Text>
                  <Text size="sm" c="dimmed">
                    →
                  </Text>
                </>
              ) : (
                <Text size="sm" c="dimmed">
                  접수 →
                </Text>
              )}
              <BookingStatusBadge status={event.to_status} />
            </Group>
          }
        >
          <Text size="xs" c="dimmed">
            {formatDateTime(event.at)} · {event.actor}
          </Text>
          {event.reason && <Text size="sm">{event.reason}</Text>}
          <Text size="xs">
            trace <TraceLink traceId={event.trace_id} at={event.at} />
          </Text>
        </Timeline.Item>
      ))}
    </Timeline>
  )
}

function JobTable({ jobs }: { jobs: Schemas['JobView'][] }) {
  const requeue = useRequeueJob()
  const canOperate = useCan('operator')
  if (jobs.length === 0) {
    return <Text c="dimmed">작업이 없습니다</Text>
  }
  return (
    <Table>
      <Table.Thead>
        <Table.Tr>
          <Table.Th>ID</Table.Th>
          <Table.Th>종류</Table.Th>
          <Table.Th>상태</Table.Th>
          <Table.Th>시도</Table.Th>
          <Table.Th>실행 시각</Table.Th>
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
              <Tooltip label={job.last_error ?? '오류 없음'} disabled={!job.last_error}>
                <span>
                  <JobStatusBadge status={job.status} />
                </span>
              </Tooltip>
            </Table.Td>
            <Table.Td>
              {job.attempts} / {job.max_attempts}
            </Table.Td>
            <Table.Td>{formatTime(job.run_at)}</Table.Td>
            <Table.Td>
              {job.status === 'dead' && (
                <NeedsRole role="operator">
                  <Button
                    size="compact-xs"
                    variant="light"
                    color="orange"
                    disabled={!canOperate}
                    loading={requeue.isPending}
                    onClick={() => requeue.mutate(job.id)}
                  >
                    다시 넣기
                  </Button>
                </NeedsRole>
              )}
              {job.sent_at && (
                <Tooltip label="멱등키 없는 업체에 보내고 아직 결과를 기록하지 못했다. 다시 가져가면 다시 보내지 않고 결과 불명으로 넘긴다">
                  <Badge size="xs" color="yellow" variant="light">
                    보냄
                  </Badge>
                </Tooltip>
              )}
            </Table.Td>
          </Table.Tr>
        ))}
      </Table.Tbody>
    </Table>
  )
}

function CallTable({ calls }: { calls: Schemas['ProviderCallView'][] }) {
  if (calls.length === 0) {
    return <Text c="dimmed">업체 호출 기록이 없습니다 (기록은 1초쯤 모아서 저장한다)</Text>
  }
  return (
    <Table.ScrollContainer minWidth={900}>
      <Table striped>
        <Table.Thead>
          <Table.Tr>
            <Table.Th>시작</Table.Th>
            <Table.Th>호출</Table.Th>
            <Table.Th>작업</Table.Th>
            <Table.Th>시도</Table.Th>
            <Table.Th>결과</Table.Th>
            <Table.Th>HTTP</Table.Th>
            <Table.Th>지연</Table.Th>
            <Table.Th>남은 한도</Table.Th>
            <Table.Th>trace</Table.Th>
          </Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {calls.map((call) => (
            <Table.Tr key={call.id}>
              <Table.Td>{formatTime(call.started_at)}</Table.Td>
              <Table.Td>
                <Code>{call.operation}</Code>
              </Table.Td>
              <Table.Td>{call.job_id ?? '–'}</Table.Td>
              <Table.Td>#{call.attempt}</Table.Td>
              <Table.Td>
                <Tooltip
                  label={call.error_detail ?? outcomeLabel(call.outcome).label}
                  multiline
                  maw={420}
                >
                  <span>
                    <OutcomeBadge outcome={call.outcome} />
                  </span>
                </Tooltip>
              </Table.Td>
              <Table.Td>{call.http_status ?? '–'}</Table.Td>
              <Table.Td>{formatMs(call.duration_ms)}</Table.Td>
              <Table.Td>
                {call.rate_limit_remaining ?? '–'}
                {call.retry_after_ms != null && (
                  <Text span size="xs" c="dimmed">
                    {' '}
                    (Retry-After {formatMs(call.retry_after_ms)})
                  </Text>
                )}
              </Table.Td>
              <Table.Td>
                <TraceLink traceId={call.trace_id} at={call.started_at} />
              </Table.Td>
            </Table.Tr>
          ))}
        </Table.Tbody>
      </Table>
    </Table.ScrollContainer>
  )
}
