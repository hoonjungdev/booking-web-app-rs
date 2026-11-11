// 화면 뼈대(머리글 + 왼쪽 메뉴)와 경로별 화면 (docs/06-admin-frontend.md §2)
//
// 로그인하지 않았으면 로그인 화면만 보여 준다 (docs/06-admin-frontend.md §6). 어느 요청이든 401을 받으면
// main.tsx가 세션 정보를 비우고, 그러면 이 컴포넌트가 다시 그려지면서 로그인 화면으로 돌아간다.
import {
  AppShell,
  Badge,
  Burger,
  Button,
  Group,
  NavLink,
  ScrollArea,
  Text,
  Title,
  Tooltip,
} from '@mantine/core'
import { useDisclosure } from '@mantine/hooks'
import {
  IconApi,
  IconBolt,
  IconBuildingStore,
  IconChartLine,
  IconFileSearch,
  IconGauge,
  IconListDetails,
  IconLogout,
  IconScale,
  IconStack2,
} from '@tabler/icons-react'
import { lazy, Suspense } from 'react'
import { Link, Route, Routes, useLocation } from 'react-router'

import { useAdminInfo } from './api/queries/info'
import { useProviders } from './api/queries/providers'
import { useIssueCounts } from './api/queries/reconciliation'
import { type Session, useLogout, useSession } from './api/queries/session'
import { ErrorAlert, Loading } from './components/QueryState'
import { dashboardUrl } from './lib/grafana'
import { ADMIN_ROLE } from './lib/labels'

// 화면별 코드는 그 화면을 처음 열 때 내려받는다 (`lazy` + 동적 `import()`).
// 차트 라이브러리처럼 무거운 코드가 첫 화면을 느리게 하지 않는다. 내려받는 동안은 `Suspense`의 fallback을 보여 준다.
const DashboardPage = lazy(() => import('./pages/DashboardPage'))
const BookingsPage = lazy(() => import('./pages/BookingsPage'))
const BookingDetailPage = lazy(() => import('./pages/BookingDetailPage'))
const ProviderPage = lazy(() => import('./pages/ProviderPage'))
const JobsPage = lazy(() => import('./pages/JobsPage'))
const ReconciliationPage = lazy(() => import('./pages/ReconciliationPage'))
const ChaosPage = lazy(() => import('./pages/ChaosPage'))
const AuditPage = lazy(() => import('./pages/AuditPage'))
const LoginPage = lazy(() => import('./pages/LoginPage'))
const NotFoundPage = lazy(() => import('./pages/NotFoundPage'))

export function App() {
  const session = useSession()

  if (session.isPending) return <Loading />
  if (session.isError) return <ErrorAlert error={session.error} title="서버에 연결할 수 없습니다" />
  if (!session.data) {
    return (
      <Suspense fallback={<Loading />}>
        <LoginPage />
      </Suspense>
    )
  }
  return <Shell session={session.data} />
}

/** 로그인한 뒤의 화면: 머리글 + 왼쪽 메뉴 + 경로별 화면 */
function Shell({ session }: { session: Session }) {
  // 좁은 화면에서 메뉴를 열고 닫는 상태 (useDisclosure: [열림 여부, { toggle, close … }])
  const [opened, { toggle, close }] = useDisclosure()

  return (
    <AppShell
      header={{ height: 56 }}
      navbar={{ width: 240, breakpoint: 'sm', collapsed: { mobile: !opened } }}
      padding="md"
    >
      <AppShell.Header>
        <Group h="100%" px="md" justify="space-between">
          <Group>
            <Burger opened={opened} onClick={toggle} hiddenFrom="sm" size="sm" />
            <Title order={4}>Booking Gateway 관리자</Title>
          </Group>
          <Group gap="md">
            <HeaderLinks />
            <UserMenu session={session} />
          </Group>
        </Group>
      </AppShell.Header>

      <AppShell.Navbar p="xs">
        <ScrollArea>
          <Navigation onNavigate={close} />
        </ScrollArea>
      </AppShell.Navbar>

      <AppShell.Main>
        {/* 주소(경로)에 맞는 화면 하나를 그린다. `:id`는 경로에서 꺼내 쓰는 값이다 (useParams) */}
        <Suspense fallback={<Loading />}>
          <Routes>
            <Route path="/" element={<DashboardPage />} />
            <Route path="/bookings" element={<BookingsPage />} />
            <Route path="/bookings/:id" element={<BookingDetailPage />} />
            <Route path="/providers/:id" element={<ProviderPage />} />
            <Route path="/jobs" element={<JobsPage />} />
            <Route path="/reconciliation" element={<ReconciliationPage />} />
            <Route path="/chaos" element={<ChaosPage />} />
            <Route path="/audit" element={<AuditPage />} />
            <Route path="*" element={<NotFoundPage />} />
          </Routes>
        </Suspense>
      </AppShell.Main>
    </AppShell>
  )
}

/** 왼쪽 메뉴. 지금 주소에 맞는 항목을 강조한다. */
function Navigation({ onNavigate }: { onNavigate: () => void }) {
  const { pathname } = useLocation()
  const providers = useProviders()
  const info = useAdminInfo()
  const issues = useIssueCounts()
  const isActive = (path: string) => (path === '/' ? pathname === '/' : pathname.startsWith(path))
  const duplicates = issues.data?.by_kind['duplicate_suspected'] ?? 0

  return (
    <>
      <NavLink
        component={Link}
        to="/"
        label="대시보드"
        leftSection={<IconGauge size={18} />}
        active={isActive('/')}
        onClick={onNavigate}
      />
      <NavLink
        component={Link}
        to="/bookings"
        label="예약"
        leftSection={<IconListDetails size={18} />}
        active={isActive('/bookings')}
        onClick={onNavigate}
      />
      <NavLink
        component={Link}
        to="/jobs"
        label="작업 큐"
        leftSection={<IconStack2 size={18} />}
        active={isActive('/jobs')}
        onClick={onNavigate}
      />
      <NavLink
        component={Link}
        to="/reconciliation"
        label="대사 이슈"
        leftSection={<IconScale size={18} />}
        rightSection={
          issues.data && issues.data.open > 0 ? (
            <Badge size="xs" color={duplicates > 0 ? 'red' : 'orange'}>
              {issues.data.open}
            </Badge>
          ) : undefined
        }
        active={isActive('/reconciliation')}
        onClick={onNavigate}
      />
      <NavLink label="업체" leftSection={<IconBuildingStore size={18} />} defaultOpened>
        {providers.data?.map((provider) => (
          <NavLink
            key={provider.id}
            component={Link}
            to={`/providers/${provider.id}`}
            label={provider.display_name}
            description={provider.id}
            active={isActive(`/providers/${provider.id}`)}
            onClick={onNavigate}
          />
        ))}
      </NavLink>
      <NavLink
        component={Link}
        to="/chaos"
        label="장애 주입"
        leftSection={<IconBolt size={18} />}
        rightSection={
          info.data && !info.data.chaos_enabled ? (
            <Badge size="xs" color="gray">
              꺼짐
            </Badge>
          ) : undefined
        }
        active={isActive('/chaos')}
        onClick={onNavigate}
      />
      <NavLink
        component={Link}
        to="/audit"
        label="감사 로그"
        leftSection={<IconFileSearch size={18} />}
        active={isActive('/audit')}
        onClick={onNavigate}
      />
    </>
  )
}

/** 로그인한 관리자와 로그아웃 */
function UserMenu({ session }: { session: Session }) {
  const logout = useLogout()
  const role = ADMIN_ROLE[session.role]
  return (
    <Group gap="xs">
      <Tooltip label={`${session.role}: ${role.description}`}>
        <Group gap={4}>
          <Text size="sm">{session.username}</Text>
          <Badge size="sm" variant="light" color={role.color}>
            {role.label}
          </Badge>
        </Group>
      </Tooltip>
      <Button
        size="compact-sm"
        variant="subtle"
        leftSection={<IconLogout size={14} />}
        loading={logout.isPending}
        onClick={() => logout.mutate()}
      >
        로그아웃
      </Button>
    </Group>
  )
}

/** 머리글 오른쪽: Grafana 대시보드와 API 문서로 가는 링크 */
function HeaderLinks() {
  const info = useAdminInfo()
  const grafana = info.data?.grafana_url
  return (
    <Group gap="md" visibleFrom="sm">
      {grafana && (
        <Text
          component="a"
          href={dashboardUrl(grafana, 'booking-provider-overview')}
          target="_blank"
          rel="noreferrer"
          size="sm"
          c="dimmed"
        >
          <IconChartLine size={14} /> Grafana
        </Text>
      )}
      {/* /docs는 화면 밖(서버)의 주소다. 개발 서버(:5173)에서는 서버(:8080)의 문서를 연다. */}
      <Text component="a" href={docsUrl()} target="_blank" rel="noreferrer" size="sm" c="dimmed">
        <IconApi size={14} /> API 문서
      </Text>
      {info.data && (
        <Text size="xs" c="dimmed">
          v{info.data.version}
        </Text>
      )}
    </Group>
  )
}

function docsUrl(): string {
  return import.meta.env.DEV ? 'http://127.0.0.1:8080/docs/' : '/docs/'
}
