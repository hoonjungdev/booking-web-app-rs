// 상태 · 결과 값의 한국어 이름과 색. 값 목록은 API 문서에서 만든 타입이라, 서버에 상태가 추가되면
// `Record<타입, …>`에 빠진 항목이 생겨 컴파일 에러가 난다 (빠뜨리지 않게 해 준다).
import type { MantineColor } from '@mantine/core'

import type { Schemas } from '../api/client'

export type BookingStatus = Schemas['BookingStatus']
export type JobStatus = Schemas['JobStatus']
export type CircuitState = Schemas['CircuitStateView']
export type ChaosPreset = Schemas['ChaosPreset']

type Label = { label: string; color: MantineColor }

export const BOOKING_STATUS: Record<BookingStatus, Label> = {
  pending: { label: '처리 중', color: 'blue' },
  confirmed: { label: '확정', color: 'green' },
  rejected: { label: '거절', color: 'orange' },
  unknown: { label: '결과 불명', color: 'yellow' },
  failed: { label: '실패', color: 'red' },
  cancel_requested: { label: '취소 중', color: 'grape' },
  cancelled: { label: '취소됨', color: 'gray' },
  cancel_failed: { label: '취소 실패', color: 'red' },
}

/** 목록 필터 순서 */
export const BOOKING_STATUSES = Object.keys(BOOKING_STATUS) as BookingStatus[]

export const JOB_STATUS: Record<JobStatus, Label> = {
  queued: { label: '대기', color: 'blue' },
  running: { label: '실행 중', color: 'cyan' },
  done: { label: '끝남', color: 'gray' },
  dead: { label: 'dead', color: 'red' },
}

export const JOB_STATUSES = Object.keys(JOB_STATUS) as JobStatus[]

export const CIRCUIT_STATE: Record<CircuitState, Label> = {
  closed: { label: '닫힘 (정상)', color: 'green' },
  half_open: { label: '반만 열림 (시험 중)', color: 'yellow' },
  open: { label: '열림 (차단)', color: 'red' },
}

/** 업체 호출 결과 (docs/03-resilience.md §1). 서버가 모르는 값을 보내도 화면이 깨지지 않게 `string`으로 받는다. */
export const OUTCOME: Record<string, Label> = {
  success: { label: '성공', color: 'green' },
  rejected: { label: '업무적 거절', color: 'teal' },
  timeout: { label: '타임아웃', color: 'orange' },
  connect_error: { label: '연결 실패', color: 'red' },
  transport_error: { label: '전송 오류', color: 'red' },
  server_error: { label: '5xx', color: 'red' },
  rate_limited: { label: '호출 제한', color: 'grape' },
  client_error: { label: '4xx', color: 'pink' },
  decode_error: { label: '응답 형식 오류', color: 'pink' },
  circuit_open: { label: '서킷 차단', color: 'gray' },
  throttled: { label: '우리 쪽 대기 초과', color: 'indigo' },
}

export function outcomeLabel(outcome: string): Label {
  return OUTCOME[outcome] ?? { label: outcome, color: 'gray' }
}

export const CHAOS_PRESET: Record<ChaosPreset, { label: string; description: string }> = {
  healthy: { label: '정상', description: '20~120ms 지연, 초당 20회' },
  flaky: { label: '간헐적 5xx', description: '10% 확률로 5xx' },
  slow: { label: '느림', description: '30% 확률로 3초 지연' },
  throttled: { label: '호출 제한', description: '초당 5회' },
  lossy: { label: '응답 유실', description: '쓰기 요청의 20%를 처리한 뒤 응답하지 않음' },
  outage: { label: '완전 장애', description: '모든 요청에 503' },
}

export const CHAOS_PRESETS = Object.keys(CHAOS_PRESET) as ChaosPreset[]
