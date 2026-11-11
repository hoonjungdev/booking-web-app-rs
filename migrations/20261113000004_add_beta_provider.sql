-- Phase 6: 로컬 개발용 mock 업체 beta (`cargo run -p mock-provider`, docs/04-api.md §4 beta)
-- 멱등키를 지원하지 않는 업체다. 다른 환경에서는 base_url을 그 환경의 주소로 바꿔 쓴다.
INSERT INTO providers (id, display_name, adapter_kind, base_url, policy)
VALUES ('beta', 'Beta (mock)', 'beta_v1', 'http://127.0.0.1:9090/beta/api', '{"attempt_timeout_ms": 3000}');
