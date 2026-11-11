// Grafana 링크 만들기 (docs/05-observability-and-analytics.md §3)
//
// 서버가 남긴 trace ID(예약 이력 · 호출 기록 · 오류 응답)를 누르면 Grafana의 Tempo 화면에서 그 trace를 연다.
// Grafana 주소는 서버 설정(BOOKING_GRAFANA_URL)에서 받는다 (`/admin/api/info`).

/** docker compose의 otel-lgtm이 등록한 Tempo 데이터 소스 ID (observability/grafana/dashboards의 JSON과 같다) */
const TEMPO_UID = 'tempo'

/** 그 일이 일어난 시각 앞뒤로 이만큼을 검색 범위로 잡는다 */
const RANGE_MS = 60 * 60 * 1000

/** trace 하나를 Grafana Explore(Tempo)에서 여는 주소. `around`: 그 일이 일어난 시각 (검색 범위를 좁힌다) */
export function traceUrl(grafanaUrl: string, traceId: string, around?: string): string {
  const center = around ? new Date(around).getTime() : Date.now()
  const panes = {
    trace: {
      datasource: TEMPO_UID,
      queries: [
        {
          refId: 'A',
          datasource: { type: 'tempo', uid: TEMPO_UID },
          queryType: 'traceql',
          query: traceId,
        },
      ],
      range: { from: String(center - RANGE_MS), to: String(center + RANGE_MS) },
    },
  }
  const params = new URLSearchParams({
    schemaVersion: '1',
    orgId: '1',
    panes: JSON.stringify(panes),
  })
  return `${grafanaUrl}/explore?${params.toString()}`
}

/** 대시보드 주소 (`uid`: observability/grafana/dashboards의 JSON에 적힌 uid) */
export function dashboardUrl(grafanaUrl: string, uid: string): string {
  return `${grafanaUrl}/d/${uid}`
}
