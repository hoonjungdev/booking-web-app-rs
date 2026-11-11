-- Phase 4: dead 작업을 빨리 찾기 위한 인덱스 (docs/05-observability-and-analytics.md §4의 `jobs.dead` 지표)
--
-- 지표를 내보내려고 몇 초마다 dead 작업 수를 센다. 끝난 작업(done)이 쌓여 테이블이 커져도 이 조회가
-- 느려지지 않도록, dead 상태인 행만 담는 부분 인덱스를 둔다. Phase 5의 관리자 화면(dead 작업 목록)도 이 인덱스를 쓴다.
CREATE INDEX jobs_dead_idx ON jobs (kind, id) WHERE status = 'dead';
