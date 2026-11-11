// 데이터를 읽는 중 · 읽기 실패를 보여 주는 부품
import { Alert, Center, Loader } from '@mantine/core'
import { IconAlertTriangle } from '@tabler/icons-react'

import { ApiError, errorMessage } from '../api/client'

export function Loading() {
  return (
    <Center py="xl">
      <Loader />
    </Center>
  )
}

/** 요청 실패. 서버가 trace ID를 알려 줬으면 함께 보여 준다 (Grafana에서 그 요청을 찾을 수 있다). */
export function ErrorAlert({ error, title = '불러오지 못했습니다' }: { error: unknown; title?: string }) {
  const traceId = error instanceof ApiError ? error.problem?.trace_id : undefined
  return (
    <Alert color="red" title={title} icon={<IconAlertTriangle />} my="md">
      {errorMessage(error)}
      {traceId && <div>trace ID: {traceId}</div>}
    </Alert>
  )
}
