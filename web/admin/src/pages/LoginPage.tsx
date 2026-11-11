// 로그인 화면 (docs/06-admin-frontend.md §6)
//
// 계정은 서버에서 `booking-server create-admin --username <이름> --role <viewer|operator|admin>`으로 만든다.
import { Alert, Button, Card, Center, PasswordInput, Stack, Text, TextInput, Title } from '@mantine/core'
import { type FormEvent, useState } from 'react'

import { errorMessage } from '../api/client'
import { useLogin } from '../api/queries/session'

export default function LoginPage() {
  // 입력 중인 값은 이 화면의 상태로 둔다 (서버에 보내기 전까지는 화면만의 값이다)
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const login = useLogin()

  const submit = (event: FormEvent) => {
    // 폼을 제출하면 브라우저가 페이지를 새로 고치려 한다. 그 대신 API로 보낸다.
    event.preventDefault()
    login.mutate({ username, password })
  }

  return (
    <Center mih="100vh" p="md">
      <Card withBorder shadow="sm" padding="xl" w={380}>
        <form onSubmit={submit}>
          <Stack>
            <Title order={3}>Booking Gateway 관리자</Title>
            <TextInput
              label="아이디"
              autoComplete="username"
              value={username}
              onChange={(event) => setUsername(event.currentTarget.value)}
              required
              autoFocus
            />
            <PasswordInput
              label="비밀번호"
              autoComplete="current-password"
              value={password}
              onChange={(event) => setPassword(event.currentTarget.value)}
              required
            />
            {login.isError && <Alert color="red">{errorMessage(login.error)}</Alert>}
            <Button type="submit" loading={login.isPending}>
              로그인
            </Button>
            <Text size="xs" c="dimmed">
              계정은 서버에서 <code>booking-server create-admin</code>으로 만듭니다.
            </Text>
          </Stack>
        </form>
      </Card>
    </Center>
  )
}
