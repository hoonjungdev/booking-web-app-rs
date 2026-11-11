// 상태 · 결과 값의 한국어 이름과 색. 값 목록은 API 문서에서 만든 타입이라, 서버에 상태가 추가되면
// `Record<타입, …>`에 빠진 항목이 생겨 컴파일 에러가 난다 (빠뜨리지 않게 해 준다).
import type { MantineColor } from '@mantine/core'

import type { Schemas } from '../api/client'

export type BookingStatus = Schemas['BookingStatus']
export type JobStatus = Schemas['JobStatus']
export type CircuitState = Schemas['CircuitStateView']
export type ChaosPreset = Schemas['ChaosPreset']
export type IssueKind = Schemas['IssueKindView']
export type AdminRole = Schemas['AdminRole']

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

/** 대사 이슈 종류 (docs/03-resilience.md §9) */
export const ISSUE_KIND: Record<IssueKind, Label & { description: string }> = {
  duplicate_suspected: {
    label: '이중 예약 의심',
    color: 'red',
    description: '같은 참조값으로 확정된 예약이 업체에 2건 이상 있다. 업체에서 하나를 취소해야 한다',
  },
  missing_at_provider: {
    label: '업체에 없음',
    color: 'orange',
    description: '우리는 확정인데 업체에는 그 예약이 없다',
  },
  missing_locally: {
    label: '우리에게 없음',
    color: 'grape',
    description: '업체에 우리 참조값을 단 예약이 있는데 우리에게는 없다',
  },
  status_mismatch: {
    label: '상태 불일치',
    color: 'yellow',
    description: '양쪽 상태가 다르다 (업체측 취소 등은 대사가 자동으로 바로잡고 처리 완료로 남긴다)',
  },
}

export const ISSUE_KINDS = Object.keys(ISSUE_KIND) as IssueKind[]

export const ADMIN_ROLE: Record<AdminRole, Label & { description: string }> = {
  viewer: { label: '조회', color: 'gray', description: '조회만' },
  operator: { label: '운영', color: 'blue', description: '재처리 · 서킷 조작 · 장애 주입 · 대사' },
  admin: { label: '관리', color: 'grape', description: '정책 변경까지' },
}
