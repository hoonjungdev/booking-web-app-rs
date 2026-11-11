// 역할에 따라 조작 버튼을 끄는 부품
import { Tooltip } from '@mantine/core'
import type { ReactElement } from 'react'

import { type AdminRole, useCan } from '../api/queries/session'
import { ADMIN_ROLE } from '../lib/labels'

/**
 * 지금 관리자의 역할이 `role`보다 낮으면 감싼 버튼 위에 이유를 툴팁으로 보여 준다.
 * 버튼 자체는 부르는 쪽이 `disabled={!useCan(role)}`로 끈다. 꺼진 버튼은 마우스 이벤트를 받지 않아서
 * 툴팁을 바로 달 수 없으므로 `<span>`으로 한 번 감싼다.
 *
 * 화면에서 끄는 것은 안내일 뿐이고, 실제 권한 검사는 서버가 한다 (403).
 */
export function NeedsRole({ role, children }: { role: AdminRole; children: ReactElement }) {
  const allowed = useCan(role)
  if (allowed) {
    return children
  }
  return (
    <Tooltip label={`${ADMIN_ROLE[role].label}(${role}) 이상의 권한이 필요합니다`}>
      <span>{children}</span>
    </Tooltip>
  )
}
