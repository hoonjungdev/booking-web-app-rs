import { Button, Stack, Text, Title } from '@mantine/core'
import { Link } from 'react-router'

export default function NotFoundPage() {
  return (
    <Stack align="flex-start">
      <Title order={2}>없는 화면입니다</Title>
      <Text c="dimmed">주소를 확인하세요.</Text>
      <Button component={Link} to="/">
        대시보드로
      </Button>
    </Stack>
  )
}
