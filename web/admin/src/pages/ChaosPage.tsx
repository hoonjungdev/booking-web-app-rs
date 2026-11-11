// 장애 주입 (개발 환경 전용): mock 업체에 장애 시나리오를 적용하고, 대시보드에서 반응을 확인한다.
//
// booking-server가 mock 업체의 `/_chaos` API를 중계한다 (BOOKING_CHAOS_URL을 설정해야 쓸 수 있다).
import {
  Alert,
  Button,
  Card,
  Code,
  Group,
  MultiSelect,
  NumberInput,
  SimpleGrid,
  Slider,
  Stack,
  Switch,
  Text,
  Title,
  Tooltip,
} from '@mantine/core'
import { useState } from 'react'

import type { Schemas } from '../api/client'
import { useApplyPreset, useChaos, useSaveChaos } from '../api/queries/chaos'
import { useAdminInfo } from '../api/queries/info'
import { useProviders } from '../api/queries/providers'
import { ErrorAlert, Loading } from '../components/QueryState'
import { CHAOS_PRESET, CHAOS_PRESETS } from '../lib/labels'

type Settings = Schemas['ChaosSettings']

export default function ChaosPage() {
  const info = useAdminInfo()
  const providers = useProviders()

  if (info.isPending || providers.isPending) return <Loading />
  if (info.isError) return <ErrorAlert error={info.error} />
  if (providers.isError) return <ErrorAlert error={providers.error} />

  return (
    <Stack gap="lg">
      <Title order={2}>장애 주입</Title>
      {!info.data.chaos_enabled ? (
        <Alert color="gray" title="장애 주입이 꺼져 있습니다">
          서버를 실행할 때 mock 업체 주소를 알려 주세요: <Code>BOOKING_CHAOS_URL=http://127.0.0.1:9090</Code> (
          <Code>.env</Code>에 넣거나 환경 변수로). 실제 업체에는 이런 API가 없으므로 개발 환경에서만 켭니다.
        </Alert>
      ) : (
        <>
          <Text c="dimmed" size="sm">
            시나리오를 적용하고 대시보드에서 서킷 · 성공률 · 지연이 어떻게 바뀌는지 확인하세요. 장애가 끝나면
            &quot;정상&quot;으로 되돌리고, 그동안 실패한 예약은 작업 큐에서 다시 넣습니다.
          </Text>
          {providers.data.map((provider) => (
            <ProviderChaos key={provider.id} provider={provider.id} name={provider.display_name} />
          ))}
        </>
      )}
    </Stack>
  )
}

function ProviderChaos({ provider, name }: { provider: string; name: string }) {
  const chaos = useChaos(provider, true)
  const preset = useApplyPreset(provider)

  return (
    <Card withBorder>
      <Title order={4} mb="sm">
        {name} <Code>{provider}</Code>
      </Title>
      {chaos.isPending && <Loading />}
      {chaos.isError && <ErrorAlert error={chaos.error} title="장애 설정을 읽지 못했습니다" />}
      {chaos.data && (
        <Stack>
          <Group gap="xs">
            {CHAOS_PRESETS.map((name) => (
              <Tooltip key={name} label={CHAOS_PRESET[name].description}>
                <Button
                  size="sm"
                  variant={name === 'healthy' ? 'filled' : 'light'}
                  color={name === 'healthy' ? 'green' : name === 'outage' ? 'red' : 'orange'}
                  loading={preset.isPending && preset.variables === name}
                  onClick={() => preset.mutate(name)}
                >
                  {CHAOS_PRESET[name].label}
                </Button>
              </Tooltip>
            ))}
          </Group>
          {/* key가 바뀌면 React가 폼을 새로 만든다: 서버 설정이 바뀌면(시나리오 적용) 입력값도 그 값으로 다시 시작한다 */}
          <ChaosForm key={JSON.stringify(chaos.data)} provider={provider} initial={chaos.data} />
        </Stack>
      )}
    </Card>
  )
}

/** 세부 설정 폼. 입력하는 동안의 값은 이 컴포넌트의 상태(useState)에 두고, 저장할 때 서버로 보낸다. */
function ChaosForm({ provider, initial }: { provider: string; initial: Settings }) {
  const [draft, setDraft] = useState<Settings>(initial)
  const save = useSaveChaos(provider)
  const changed = JSON.stringify(draft) !== JSON.stringify(initial)

  /** 일부 값만 바꾼다 (객체를 통째로 새로 만들어야 React가 바뀐 것을 안다) */
  const update = (patch: Partial<Settings>) => setDraft((current) => ({ ...current, ...patch }))

  return (
    <Stack>
      <Switch
        label="완전 장애 (모든 요청에 503)"
        color="red"
        checked={draft.outage}
        onChange={(event) => update({ outage: event.currentTarget.checked })}
      />
      <SimpleGrid cols={{ base: 1, md: 2 }} spacing="lg">
        <Group grow>
          <NumberInput
            label="기본 지연 최소 (ms)"
            min={0}
            value={draft.latency_ms.min}
            onChange={(value) => update({ latency_ms: { ...draft.latency_ms, min: Number(value) || 0 } })}
          />
          <NumberInput
            label="기본 지연 최대 (ms)"
            min={0}
            value={draft.latency_ms.max}
            onChange={(value) => update({ latency_ms: { ...draft.latency_ms, max: Number(value) || 0 } })}
          />
        </Group>
        <Group grow>
          <NumberInput
            label="호출 한도: 초당"
            min={1}
            value={draft.rate_limit.per_second}
            onChange={(value) =>
              update({ rate_limit: { ...draft.rate_limit, per_second: Number(value) || 1 } })
            }
          />
          <NumberInput
            label="호출 한도: 버킷"
            min={1}
            value={draft.rate_limit.burst}
            onChange={(value) => update({ rate_limit: { ...draft.rate_limit, burst: Number(value) || 1 } })}
          />
        </Group>
        <Probability
          label="느린 응답 확률"
          value={draft.slow.probability}
          onChange={(probability) => update({ slow: { ...draft.slow, probability } })}
        />
        <NumberInput
          label="느린 응답에 더할 지연 (ms)"
          min={0}
          value={draft.slow.latency_ms}
          onChange={(value) => update({ slow: { ...draft.slow, latency_ms: Number(value) || 0 } })}
        />
        <Probability
          label="5xx 확률 (예약을 만들지 않음)"
          value={draft.error.probability}
          onChange={(probability) => update({ error: { ...draft.error, probability } })}
        />
        <MultiSelect
          label="5xx 상태 코드"
          data={['500', '502', '503', '504']}
          value={draft.error.statuses.map(String)}
          onChange={(values) => update({ error: { ...draft.error, statuses: values.map(Number) } })}
        />
        <Probability
          label="응답 없음 확률 (30초 대기 → 타임아웃)"
          value={draft.hang_probability}
          onChange={(hang_probability) => update({ hang_probability })}
        />
        <Probability
          label="처리 후 응답 유실 확률 (쓰기 요청, 결과 불명)"
          value={draft.drop_after_commit_probability}
          onChange={(drop_after_commit_probability) => update({ drop_after_commit_probability })}
        />
      </SimpleGrid>
      <Group>
        <Button onClick={() => save.mutate(draft)} loading={save.isPending} disabled={!changed}>
          저장
        </Button>
        <Button variant="subtle" onClick={() => setDraft(initial)} disabled={!changed}>
          되돌리기
        </Button>
      </Group>
    </Stack>
  )
}

/** 0~1 확률 슬라이더 */
function Probability({
  label,
  value,
  onChange,
}: {
  label: string
  value: number
  onChange: (value: number) => void
}) {
  return (
    <Stack gap={4}>
      <Text size="sm" fw={500}>
        {label}: {Math.round(value * 100)}%
      </Text>
      <Slider
        min={0}
        max={1}
        step={0.05}
        value={value}
        onChange={onChange}
        label={(v) => `${Math.round(v * 100)}%`}
        marks={[
          { value: 0, label: '0%' },
          { value: 0.5, label: '50%' },
          { value: 1, label: '100%' },
        ]}
        mb="md"
      />
    </Stack>
  )
}
