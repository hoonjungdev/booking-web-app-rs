// 감사 로그: 관리자 조작 기록 (로그인 · 재시도 · 재큐잉 · 서킷 조작 · 정책 변경 · 장애 주입 · 대사)
//
// 예약 상태를 바꾼 조작은 예약 상세의 상태 이력에도 `admin:<이름>`으로 남는다.
import { Button, Code, Stack, Table, Text, Title, Tooltip } from '@mantine/core'

import { useAuditLog } from '../api/queries/audit'
import { ErrorAlert, Loading } from '../components/QueryState'
import { TraceLink } from '../components/TraceLink'
import { formatDateTime } from '../lib/format'

export default function AuditPage() {
  const log = useAuditLog()
  const entries = log.data?.pages.flatMap((page) => page.items) ?? []

  return (
    <Stack>
      <Title order={2}>감사 로그</Title>
      <Text c="dimmed" size="sm">
        관리자가 무엇을 언제 했는지 남긴 기록입니다. trace를 누르면 그 조작 요청의 처리 과정을 Grafana에서 엽니다.
      </Text>
      {log.isPending && <Loading />}
      {log.isError && <ErrorAlert error={log.error} />}
      {log.data && entries.length === 0 && <Text c="dimmed">기록이 없습니다</Text>}
      {entries.length > 0 && (
        <Table.ScrollContainer minWidth={800}>
          <Table striped>
            <Table.Thead>
              <Table.Tr>
                <Table.Th>시각</Table.Th>
                <Table.Th>관리자</Table.Th>
                <Table.Th>조작</Table.Th>
                <Table.Th>대상</Table.Th>
                <Table.Th>내용</Table.Th>
                <Table.Th>trace</Table.Th>
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {entries.map((entry) => (
                <Table.Tr key={entry.id}>
                  <Table.Td>{formatDateTime(entry.at)}</Table.Td>
                  <Table.Td>{entry.actor}</Table.Td>
                  <Table.Td>
                    <Code>{entry.action}</Code>
                  </Table.Td>
                  <Table.Td>{entry.target ?? '–'}</Table.Td>
                  <Table.Td>
                    <Tooltip
                      label={<pre style={{ margin: 0 }}>{JSON.stringify(entry.detail, null, 2)}</pre>}
                      multiline
                    >
                      <Text size="sm" lineClamp={1} maw={360} ff="monospace">
                        {JSON.stringify(entry.detail)}
                      </Text>
                    </Tooltip>
                  </Table.Td>
                  <Table.Td>
                    <TraceLink traceId={entry.trace_id} at={entry.at} />
                  </Table.Td>
                </Table.Tr>
              ))}
            </Table.Tbody>
          </Table>
        </Table.ScrollContainer>
      )}
      {log.hasNextPage && (
        <Button variant="light" onClick={() => log.fetchNextPage()} loading={log.isFetchingNextPage} w="fit-content">
          더 보기
        </Button>
      )}
    </Stack>
  )
}
