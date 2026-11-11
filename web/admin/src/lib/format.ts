// 화면에 보여 줄 값의 서식. API의 시각은 UTC(RFC 3339)이고, 화면에는 브라우저의 시간대(한국이면 KST)로 보여 준다.

// 'sv-SE'(스웨덴어) 서식은 "2026-11-11 20:58:12"처럼 ISO와 비슷한 모양이라 표에 쓰기 좋다
const dateTime = new Intl.DateTimeFormat('sv-SE', {
  year: 'numeric',
  month: '2-digit',
  day: '2-digit',
  hour: '2-digit',
  minute: '2-digit',
  second: '2-digit',
})
const time = new Intl.DateTimeFormat('sv-SE', {
  hour: '2-digit',
  minute: '2-digit',
  second: '2-digit',
})
const shortTime = new Intl.DateTimeFormat('sv-SE', { hour: '2-digit', minute: '2-digit' })
const relative = new Intl.RelativeTimeFormat('ko', { numeric: 'auto' })

/** 2026-11-11 20:58:12 */
export function formatDateTime(iso: string): string {
  return dateTime.format(new Date(iso))
}

/** 20:58:12 */
export function formatTime(iso: string): string {
  return time.format(new Date(iso))
}

/** 20:58 (차트의 가로축) */
export function formatShortTime(iso: string): string {
  return shortTime.format(new Date(iso))
}

/** "3분 전", "12초 후" */
export function formatRelative(iso: string, now: number = Date.now()): string {
  const seconds = Math.round((new Date(iso).getTime() - now) / 1000)
  const abs = Math.abs(seconds)
  if (abs < 60) return relative.format(seconds, 'second')
  if (abs < 3600) return relative.format(Math.round(seconds / 60), 'minute')
  if (abs < 86400) return relative.format(Math.round(seconds / 3600), 'hour')
  return relative.format(Math.round(seconds / 86400), 'day')
}

/** 84 ms, 1.25 s. 값이 없으면 "–" */
export function formatMs(ms: number | null | undefined): string {
  if (ms == null) return '–'
  if (ms < 1000) return `${Math.round(ms)} ms`
  return `${(ms / 1000).toFixed(2)} s`
}

/** 99.2%. 값이 없으면 "–" */
export function formatPercent(ratio: number | null | undefined): string {
  if (ratio == null) return '–'
  return `${(ratio * 100).toFixed(1)}%`
}

/** 1,234 */
export function formatCount(n: number): string {
  return n.toLocaleString('ko-KR')
}

/**
 * 표에서 짧게 보여 줄 ID: UUID의 끝 8자리. 전체 값은 툴팁이나 상세 화면에서 본다.
 * 예약 ID는 UUID v7이라 앞부분이 생성 시각이다. 비슷한 때 만든 예약은 앞자리가 같아서, 무작위인 끝부분으로 구분한다.
 */
export function shortId(id: string): string {
  return `…${id.slice(-8)}`
}
