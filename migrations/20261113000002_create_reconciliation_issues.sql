-- Phase 6: 대사(reconciliation)에서 찾은 불일치 (docs/02-domain-and-data.md §4, docs/03-resilience.md §9)
--
-- 대사가 자동으로 바로잡은 불일치(업체측 취소 → cancelled 등)도 기록하되, 처리 완료(resolved_at)로 넣는다.
-- 자동으로 처리하지 않는 불일치(업체에 없음, 이중 예약 의심 등)는 관리자가 확인하고 처리 완료로 표시한다.

CREATE TABLE reconciliation_issues (
    id                   bigserial PRIMARY KEY,
    provider_id          text NOT NULL REFERENCES providers (id),
    -- 우리 예약. 우리 쪽에 없는 예약(missing_locally)이면 업체가 알려 준 참조값을 그대로 넣으므로 외래 키를 걸지 않는다.
    booking_id           uuid,
    provider_booking_id  text,
    kind                 text NOT NULL,
    -- 무엇이 어떻게 달랐는지 (우리 상태, 업체 상태, 업체 예약 ID 목록 등). 고객 정보는 넣지 않는다.
    detail               jsonb NOT NULL DEFAULT '{}',
    detected_at          timestamptz NOT NULL DEFAULT now(),
    resolved_at          timestamptz,
    -- 'reconciler'(자동으로 바로잡음) 또는 'admin:<이름>'
    resolved_by          text,
    CONSTRAINT reconciliation_issues_kind_check CHECK (kind IN (
        'missing_at_provider', 'missing_locally', 'status_mismatch', 'duplicate_suspected'
    ))
);

-- 같은 불일치를 대사가 돌 때마다(10분) 새로 쌓지 않는다: 미해결 이슈는 (업체, 종류, 예약, 업체 예약)마다 하나.
-- NULLS NOT DISTINCT: 값이 없는(NULL) 칸끼리도 같은 값으로 본다 (PostgreSQL 15+). 그래야 booking_id만 있는 이슈도 겹치지 않는다.
CREATE UNIQUE INDEX reconciliation_issues_open_idx
    ON reconciliation_issues (provider_id, kind, booking_id, provider_booking_id) NULLS NOT DISTINCT
    WHERE resolved_at IS NULL;
CREATE INDEX reconciliation_issues_booking_idx ON reconciliation_issues (booking_id)
    WHERE booking_id IS NOT NULL;
