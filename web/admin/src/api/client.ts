// API 호출 도구
//
// `schema.d.ts`는 백엔드의 API 문서(openapi.json)에서 자동으로 만든 타입이다 (직접 고치지 않는다).
// openapi-fetch가 이 타입으로 경로 · 매개변수 · 응답을 검사하므로, 백엔드 API가 바뀌면 여기서 컴파일 에러가 난다.
//   pnpm gen  →  booking-server openapi > openapi.json  →  openapi-typescript  →  src/api/schema.d.ts
import createClient from 'openapi-fetch'

import type { components, paths } from './schema'

/** API 문서의 스키마 타입들. `Schemas['AdminBooking']`처럼 쓴다. */
export type Schemas = components['schemas']

/**
 * API 클라이언트. 주소를 적지 않으면 지금 페이지와 같은 주소로 보낸다.
 * 배포: booking-server가 화면과 API를 함께 내려준다. 개발: Vite가 /admin/api를 서버로 넘겨 준다 (vite.config.ts).
 */
export const api = createClient<paths>()

/** API가 돌려준 오류. 서버는 RFC 9457 Problem Details 형식으로 오류를 돌려준다. */
export class ApiError extends Error {
  readonly status: number
  readonly problem?: Schemas['Problem']

  constructor(status: number, problem?: Schemas['Problem']) {
    super(problem?.detail ?? `요청이 실패했습니다 (HTTP ${status})`)
    this.name = 'ApiError'
    this.status = status
    this.problem = problem
  }
}

/** openapi-fetch의 결과 모양: 성공이면 `data`, 실패면 `error`가 들어 있다 */
type FetchResult<T> = { data?: T; error?: unknown; response: Response }

/**
 * 요청 결과에서 데이터를 꺼낸다. 실패했으면 `ApiError`를 던진다.
 *
 * TanStack Query는 던져진 에러를 받아 `error` 상태로 만들어 준다. 그래서 화면 코드는
 * `if (query.isError)`로 실패를 다루고, 성공한 `query.data`에는 항상 값이 들어 있다고 믿을 수 있다.
 */
export async function unwrap<T>(request: Promise<FetchResult<T>>): Promise<T> {
  const { data, error, response } = await request
  if (response.ok) {
    return data as T
  }
  throw new ApiError(response.status, isProblem(error) ? error : undefined)
}

function isProblem(value: unknown): value is Schemas['Problem'] {
  return typeof value === 'object' && value !== null && 'detail' in value && 'status' in value
}

/** 화면에 보여 줄 오류 문장 */
export function errorMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message
  }
  return String(error)
}
