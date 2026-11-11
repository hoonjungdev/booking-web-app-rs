// 예약 목록: 상태 · 업체 · 기간 필터, 커서 페이지네이션 ("더 보기")
//
// 필터는 주소의 쿼리 문자열(?status=failed)에 둔다. 새로 고치거나 주소를 공유해도 같은 목록이 나오고,
// 대시보드의 "모두 보기"처럼 다른 화면에서 필터를 정해 들어올 수 있다.
import {
  Anchor,
  Button,
  Group,
  SegmentedControl,
  Select,
  Stack,
  Table,
  Text,
  Title,
  Tooltip,
} from '@mantine/core'
import { Link, useSearchParams } from 'react-router'

import { type BookingFilters, useBookingList } from '../api/queries/bookings'
import { useProviders } from '../api/queries/providers'
import { BookingStatusBadge } from '../components/Badges'
import { ErrorAlert, Loading } from '../components/QueryState'
import { formatDateTime, shortId } from '../lib/format'
import { BOOKING_STATUS, BOOKING_STATUSES, type BookingStatus } from '../lib/labels'

/** 기간 필터: 접수 시각 기준 */
const PERIODS = [
  { value: 'all', label: '전체', hours: 0 },
  { value: '1h', label: '1시간', hours: 1 },
  { value: '24h', label: '24시간', hours: 24 },
  { value: '7d', label: '7일', hours: 24 * 7 },
] as const

export default function BookingsPage() {
  const [params, setParams] = useSearchParams()
  const filters: BookingFilters = {
    status: (params.get('status') as BookingStatus | null) ?? undefined,
    provider: params.get('provider') ?? undefined,
    from: params.get('from') ?? undefined,
  }
  const period = params.get('period') ?? 'all'

  const providers = useProviders()
  const list = useBookingList(filters)
  const bookings = list.data?.pages.flatMap((page) => page.items) ?? []

  /** 필터 하나를 바꾼다 (값이 없으면 주소에서 뺀다) */
  const setFilter = (key: string, value: string | null) => {
    const next = new URLSearchParams(params)
    if (value) {
      next.set(key, value)
    } else {
      next.delete(key)
    }
    setParams(next)
  }

  /**
   * 기간을 고르면 그 순간을 기준으로 시작 시각을 계산해 주소에 함께 넣는다.
   * 화면을 그릴 때마다 계산하면 시각이 계속 흘러 조건이 매번 달라지므로(목록을 끝없이 다시 읽는다),
   * 사용자가 고른 순간(이벤트 처리 함수)에 한 번만 계산한다.
   */
  const setPeriod = (value: string) => {
    const next = new URLSearchParams(params)
    const hours = PERIODS.find((p) => p.value === value)?.hours ?? 0
    if (hours > 0) {
      next.set('period', value)
      next.set('from', new Date(Date.now() - hours * 3_600_000).toISOString())
    } else {
      next.delete('period')
      next.delete('from')
    }
    setParams(next)
  }

  return (
    <Stack>
      <Title order={2}>예약</Title>

      <Group align="flex-end">
        <Select
          label="상태"
          placeholder="전체"
          clearable
          data={BOOKING_STATUSES.map((s) => ({ value: s, label: `${BOOKING_STATUS[s].label} (${s})` }))}
          value={filters.status ?? null}
          onChange={(value) => setFilter('status', value)}
          w={220}
        />
        <Select
          label="업체"
          placeholder="전체"
          clearable
          data={providers.data?.map((p) => ({ value: p.id, label: p.display_name })) ?? []}
          value={filters.provider ?? null}
          onChange={(value) => setFilter('provider', value)}
          w={200}
        />
        <Stack gap={4}>
          <Text size="sm" fw={500}>
            접수 시각
          </Text>
          <SegmentedControl
            data={PERIODS.map(({ value, label }) => ({ value, label }))}
            value={period}
            onChange={setPeriod}
          />
        </Stack>
      </Group>

      {list.isPending && <Loading />}
      {list.isError && <ErrorAlert error={list.error} />}
      {list.data && bookings.length === 0 && (
        <Text c="dimmed" py="lg">
          조건에 맞는 예약이 없습니다
        </Text>
      )}
      {bookings.length > 0 && (
        <Table.ScrollContainer minWidth={900}>
          <Table striped highlightOnHover>
            <Table.Thead>
              <Table.Tr>
                <Table.Th>접수 시각</Table.Th>
                <Table.Th>예약 ID</Table.Th>
                <Table.Th>업체</Table.Th>
                <Table.Th>자원</Table.Th>
                <Table.Th>슬롯</Table.Th>
                <Table.Th>인원</Table.Th>
                <Table.Th>상태</Table.Th>
                <Table.Th>사유</Table.Th>
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {bookings.map((booking) => (
                <Table.Tr key={booking.id}>
                  <Table.Td>{formatDateTime(booking.created_at)}</Table.Td>
                  <Table.Td>
                    <Tooltip label={booking.id}>
                      <Anchor component={Link} to={`/bookings/${booking.id}`} ff="monospace">
                        {shortId(booking.id)}
                      </Anchor>
                    </Tooltip>
                  </Table.Td>
                  <Table.Td>{booking.provider}</Table.Td>
                  <Table.Td>{booking.resource_id}</Table.Td>
                  <Table.Td>{formatDateTime(booking.slot_start)}</Table.Td>
                  <Table.Td>{booking.party_size}</Table.Td>
                  <Table.Td>
                    <BookingStatusBadge status={booking.status} />
                  </Table.Td>
                  <Table.Td>
                    <Text size="sm" lineClamp={1} maw={260}>
                      {booking.status_reason ?? ''}
                    </Text>
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
