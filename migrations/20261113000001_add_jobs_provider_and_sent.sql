-- Phase 6: 워커의 업체별 동시 처리 제한, 멱등키 없는 업체의 "보냄" 표시, 대사 작업 (docs/decisions.md D-30, D-31, D-33)

-- 작업이 어느 업체를 부르는가. 워커는 업체마다 따로 둔 루프로 그 업체의 작업만 가져간다 (한 업체가 느려도 다른 업체 작업이 밀리지 않게).
-- 예약 작업은 예약의 업체, 대사 작업은 대사할 업체다. 업체와 상관없는 작업은 NULL.
ALTER TABLE jobs ADD COLUMN provider_id text REFERENCES providers (id);
UPDATE jobs SET provider_id = b.provider_id FROM bookings b WHERE b.id = jobs.booking_id;

-- 멱등키 없는 업체에 생성 요청을 보내기 직전에 커밋하는 표시. 결과를 기록하면 지운다.
-- 다시 가져간 작업에 이 표시가 남아 있으면, 앞선 워커가 보낸 뒤 결과를 남기지 못하고 멈춘 것이다 → 다시 보내지 않고 결과 불명으로.
ALTER TABLE jobs ADD COLUMN sent_at timestamptz;

-- 업체별 루프가 실행할 작업을 찾는 인덱스 (jobs_ready_idx의 업체별 판)
CREATE INDEX jobs_provider_ready_idx ON jobs (provider_id, run_at) WHERE status = 'queued';

-- 업체마다 대사 작업은 하나만 대기 · 실행 중일 수 있다. 정기 실행을 등록할 때 이미 있으면 새로 넣지 않는다 (ON CONFLICT DO NOTHING).
CREATE UNIQUE INDEX jobs_reconcile_active_idx ON jobs (provider_id)
    WHERE kind = 'reconcile_provider' AND status IN ('queued', 'running');
