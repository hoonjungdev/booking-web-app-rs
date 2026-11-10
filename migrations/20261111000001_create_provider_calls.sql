-- Phase 3: 외부 업체 호출 기록 (docs/02-domain-and-data.md §4, docs/05-observability-and-analytics.md §6)
--
-- 시도 1번 = 1행. 통계 · 분석의 원천 데이터다. 호출 경로를 느리게 하지 않도록 서버가 채널로 모아 배치로 넣는다.
-- 기록 테이블이라 예약이 지워져도(보관 정책) 남을 수 있게 bookings에 외래 키를 걸지 않는다.

CREATE TABLE provider_calls (
    id                    bigserial PRIMARY KEY,
    provider_id           text NOT NULL,
    -- create_booking, find_booking (Phase 6: cancel_booking, availability, list_bookings)
    operation             text NOT NULL,
    booking_id            uuid,
    job_id                bigint,
    -- 호출 내 재시도 번호 (1부터)
    attempt               integer NOT NULL,
    started_at            timestamptz NOT NULL,
    duration_ms           integer NOT NULL,
    -- success 또는 docs/03-resilience.md §1의 분류 (timeout, server_error, circuit_open …)
    outcome               text NOT NULL,
    http_status           integer,
    -- 업체 응답 헤더 X-RateLimit-Remaining
    rate_limit_remaining  integer,
    -- 업체 응답 헤더 Retry-After
    retry_after_ms        integer,
    error_detail          text,
    -- Phase 4 (OpenTelemetry)에서 채운다
    trace_id              text
);
CREATE INDEX provider_calls_time_idx ON provider_calls (provider_id, started_at);
CREATE INDEX provider_calls_booking_idx ON provider_calls (booking_id) WHERE booking_id IS NOT NULL;
