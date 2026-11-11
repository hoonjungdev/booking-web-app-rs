// 상태를 색으로 보여 주는 배지들
import { Badge, Group, Tooltip } from '@mantine/core'

import type { Schemas } from '../api/client'
import {
  BOOKING_STATUS,
  type BookingStatus,
  CIRCUIT_STATE,
  JOB_STATUS,
  type JobStatus,
  outcomeLabel,
} from '../lib/labels'

export function BookingStatusBadge({ status }: { status: BookingStatus }) {
  const { label, color } = BOOKING_STATUS[status]
  return (
    <Tooltip label={status}>
      <Badge color={color} variant="light">
        {label}
      </Badge>
    </Tooltip>
  )
}

export function JobStatusBadge({ status }: { status: JobStatus }) {
  const { label, color } = JOB_STATUS[status]
  return (
    <Badge color={color} variant={status === 'dead' ? 'filled' : 'light'}>
      {label}
    </Badge>
  )
}

/** 업체 호출 결과 (success, timeout, circuit_open …) */
export function OutcomeBadge({ outcome }: { outcome: string }) {
  const { label, color } = outcomeLabel(outcome)
  return (
    <Tooltip label={outcome}>
      <Badge color={color} variant="light">
        {label}
      </Badge>
    </Tooltip>
  )
}

/** 서킷 상태. 관리자가 강제로 정한 상태면 "강제" 표시를 함께 단다. */
export function CircuitBadge({ circuit }: { circuit: Schemas['CircuitView'] }) {
  const { label, color } = CIRCUIT_STATE[circuit.state]
  return (
    <Group gap={4}>
      <Badge color={color} variant="filled">
        {label}
      </Badge>
      {circuit.forced && (
        <Tooltip label="관리자가 강제로 정한 상태입니다. 해제할 때까지 호출 결과로 바뀌지 않습니다.">
          <Badge color="dark" variant="outline">
            강제
          </Badge>
        </Tooltip>
      )}
    </Group>
  )
}
