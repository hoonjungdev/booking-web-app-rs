import { Anchor, Code, Tooltip } from '@mantine/core'
import { IconExternalLink } from '@tabler/icons-react'

import { useAdminInfo } from '../api/queries/info'
import { traceUrl } from '../lib/grafana'

type Props = {
  traceId: string | null | undefined
  /** 그 일이 일어난 시각. Grafana의 검색 범위를 좁힌다. */
  at?: string
}

/**
 * trace ID. Grafana 주소를 알면 누를 수 있는 링크로 보여 준다 (그 요청 · 작업의 처리 과정을 Tempo에서 연다).
 * 컴포넌트 안에서 `useAdminInfo()`를 부르지만 같은 데이터는 한 번만 받아 온다 (TanStack Query가 캐시를 공유한다).
 */
export function TraceLink({ traceId, at }: Props) {
  const info = useAdminInfo()
  if (!traceId) {
    return <span>–</span>
  }
  const short = traceId.slice(0, 12)
  const grafana = info.data?.grafana_url
  if (!grafana) {
    return (
      <Tooltip label={traceId}>
        <Code>{short}</Code>
      </Tooltip>
    )
  }
  return (
    <Tooltip label={`Grafana에서 trace 열기: ${traceId}`}>
      <Anchor href={traceUrl(grafana, traceId, at)} target="_blank" rel="noreferrer" size="sm">
        <Code>{short}</Code> <IconExternalLink size={12} />
      </Anchor>
    </Tooltip>
  )
}
