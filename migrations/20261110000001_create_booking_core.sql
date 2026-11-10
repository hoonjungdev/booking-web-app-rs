-- Phase 2: 예약 접수와 처리에 필요한 테이블 (docs/02-domain-and-data.md §4)
--
-- 이미 적용된 마이그레이션은 고치지 않는다. 바꿀 것이 있으면 새 파일을 추가한다.
-- provider_calls, reconciliation_issues 테이블은 그 기능을 만드는 Phase에서 별도 마이그레이션으로 추가한다.

-- 우리 예약 API를 호출하는 내부 클라이언트. API 키는 원문 대신 SHA-256 해시만 저장한다.
CREATE TABLE api_clients (
    id            uuid PRIMARY KEY,
    name          text NOT NULL,
    api_key_hash  text NOT NULL UNIQUE,
    created_at    timestamptz NOT NULL DEFAULT now()
);

-- 외부 업체와 업체별 정책
CREATE TABLE providers (
    id            text PRIMARY KEY,
    display_name  text NOT NULL,
    adapter_kind  text NOT NULL,
    base_url      text NOT NULL,
    enabled       boolean NOT NULL DEFAULT true,
    policy        jsonb NOT NULL DEFAULT '{}',
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE bookings (
    id                   uuid PRIMARY KEY,
    client_id            uuid NOT NULL REFERENCES api_clients (id),
    idempotency_key      text NOT NULL,
    request_hash         bytea NOT NULL,
    provider_id          text NOT NULL REFERENCES providers (id),
    resource_id          text NOT NULL,
    slot_start           timestamptz NOT NULL,
    party_size           integer NOT NULL CHECK (party_size > 0),
    customer             jsonb NOT NULL,
    status               text NOT NULL,
    status_reason        text,
    provider_booking_id  text,
    deadline_at          timestamptz NOT NULL,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT bookings_idempotency_key UNIQUE (client_id, idempotency_key),
    CONSTRAINT bookings_status_check CHECK (status IN (
        'pending', 'confirmed', 'rejected', 'unknown', 'failed',
        'cancel_requested', 'cancelled', 'cancel_failed'
    ))
);
CREATE INDEX bookings_status_idx ON bookings (status, updated_at);
CREATE INDEX bookings_provider_idx ON bookings (provider_id, slot_start);

-- 상태 변경 이력
CREATE TABLE booking_events (
    id           bigserial PRIMARY KEY,
    booking_id   uuid NOT NULL REFERENCES bookings (id),
    from_status  text,
    to_status    text NOT NULL,
    reason       text,
    actor        text NOT NULL,
    trace_id     text,
    at           timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX booking_events_booking_idx ON booking_events (booking_id, at);

-- 작업 큐 (Transactional Outbox 역할도 함)
CREATE TABLE jobs (
    id             bigserial PRIMARY KEY,
    kind           text NOT NULL,
    booking_id     uuid REFERENCES bookings (id),
    payload        jsonb NOT NULL DEFAULT '{}',
    status         text NOT NULL DEFAULT 'queued',
    attempts       integer NOT NULL DEFAULT 0,
    max_attempts   integer NOT NULL,
    run_at         timestamptz NOT NULL DEFAULT now(),
    locked_until   timestamptz,
    last_error     text,
    trace_context  jsonb,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT jobs_status_check CHECK (status IN ('queued', 'running', 'done', 'dead'))
);
-- 실행할 작업 찾기용 (queued)과, 처리 도중 죽은 워커의 작업 회수용 (running + 만료된 lease)
CREATE INDEX jobs_ready_idx ON jobs (run_at) WHERE status = 'queued';
CREATE INDEX jobs_lease_idx ON jobs (locked_until) WHERE status = 'running';

-- 로컬 개발용 mock 업체 alpha (`cargo run -p mock-provider`).
-- 다른 환경에서는 base_url을 그 환경의 주소로 바꿔 쓴다.
INSERT INTO providers (id, display_name, adapter_kind, base_url, policy)
VALUES ('alpha', 'Alpha (mock)', 'alpha_v1', 'http://127.0.0.1:9090/alpha/v1', '{"attempt_timeout_ms": 3000}');
