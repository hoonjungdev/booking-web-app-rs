-- Phase 6: 관리자 인증 · 권한 · 감사 로그 (docs/06-admin-frontend.md §6, docs/decisions.md D-36)

-- 관리자 계정. 비밀번호는 argon2 해시(PHC 문자열)만 저장한다.
CREATE TABLE admin_users (
    id             uuid PRIMARY KEY,
    username       text NOT NULL UNIQUE,
    password_hash  text NOT NULL,
    -- viewer(조회만) < operator(재처리 · 서킷 조작 · 장애 주입 · 대사) < admin(정책 변경)
    role           text NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT admin_users_role_check CHECK (role IN ('viewer', 'operator', 'admin'))
);

-- 로그인 세션. 쿠키에는 256비트 무작위 토큰을 주고, DB에는 그 SHA-256 해시만 둔다 (API 키와 같은 방식, D-14).
CREATE TABLE admin_sessions (
    token_hash  text PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES admin_users (id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL
);
CREATE INDEX admin_sessions_user_idx ON admin_sessions (user_id);

-- 관리자 조작 기록. 예약 상태를 바꾼 조작은 booking_events.actor에도 'admin:<이름>'으로 남는다.
CREATE TABLE admin_audit_log (
    id        bigserial PRIMARY KEY,
    -- 'admin:<이름>'
    actor     text NOT NULL,
    -- 'booking.retry', 'provider.policy.update' …
    action    text NOT NULL,
    -- 조작 대상 (예약 ID, 업체 ID, 작업 ID 등)
    target    text,
    detail    jsonb NOT NULL DEFAULT '{}',
    trace_id  text,
    at        timestamptz NOT NULL DEFAULT now()
);
