# 설계 결정 기록 (ADR)

결정을 바꿀 때는 기존 항목을 지우지 않는다. 상태를 `대체됨 (→ D-xx)`으로 바꾸고 새 항목을 추가한다.

### D-01. 백엔드: Rust + tokio + axum
- 상태: 채택
- 이유: tokio 팀이 관리하는 사실상 표준 조합이다. tower 미들웨어 생태계를 서버(들어오는 요청)와 업체 호출(나가는 요청) 양쪽에 같은 방식으로 쓸 수 있다.
- 대안: actix-web (독자 생태계), Loco (추상화가 많아 학습용으로 맞지 않음)

### D-02. DB: PostgreSQL + sqlx, ORM 없음
- 상태: 채택
- 이유: SQL을 직접 쓰되 컴파일 시점에 실제 스키마로 검사한다. `FOR UPDATE SKIP LOCKED`, `percentile_cont` 같은 PostgreSQL 기능을 그대로 쓸 수 있다.
- 결과: 빌드할 때 DB 연결이나 오프라인 메타데이터(`.sqlx/`, 커밋함)가 필요하다.
- 대안: SeaORM, Diesel

### D-03. 예약 생성은 비동기(202) + PostgreSQL 작업 큐
- 상태: 채택
- 이유: 업체가 느리거나 죽어 있어도 접수는 바로 성공한다. 예약 저장과 작업 등록이 한 트랜잭션이라 유실이 없다(Transactional Outbox). 메시지 브로커 없이 인프라를 단순하게 유지한다.
- 결과: 클라이언트는 폴링으로 결과를 확인한다. 처리량이 아주 커지면 브로커 도입을 다시 검토한다.
- 대안: 동기 호출 (업체 장애가 곧 우리 장애가 됨), RabbitMQ / Kafka (학습 범위에 비해 과함)

### D-04. 단일 바이너리, 역할 선택 (`--role all|api|worker`)
- 상태: 채택
- 이유: 로컬 실행과 배포가 단순하다. 나중에 API와 워커를 따로 띄울 수 있는 길은 열어 둔다.

### D-05. 장애 대응은 작은 부품을 조합하고, 서킷 브레이커와 재시도는 직접 구현
- 상태: 채택
- 이유: 동작을 완전히 이해하고 원하는 곳에 관측 지점을 넣기 위해서다. 학습 가치도 크다. rate limit만은 검증된 governor를 쓴다.
- 대안: failsafe 크레이트, tower의 retry 계층 그대로 사용

### D-06. 관측은 OpenTelemetry(OTLP) 단일 경로, 로컬은 grafana/otel-lgtm
- 상태: 채택
- 이유: 특정 업체에 묶이지 않는다. Datadog이나 OpenObserve 등으로 바꿔도 코드를 고치지 않는다. Docker 컨테이너 하나로 추적 · 메트릭 · 로그 · 대시보드가 모두 뜬다.

### D-07. 업체 호출 통계는 메트릭과 DB 호출 기록 두 갈래로
- 상태: 채택
- 이유: 메트릭은 실시간이고 저렴하지만 p95가 근사값이고 조건 분석이 제한적이다. 호출 기록은 정확하고 예약 단위로 추적할 수 있다.
- 결과: `provider_calls`의 보관 기간(30일)을 관리해야 한다.

### D-08. 관리자 웹: React + TypeScript + Vite + Mantine + TanStack Query, 타입은 OpenAPI에서 생성
- 상태: 채택
- 이유: React 기초를 이미 알고 있고, 백엔드 학습에 집중하기 위해 프론트는 생산성 높은 조합을 쓴다. 타입 자동 생성으로 백엔드와 프론트의 계약 불일치를 컴파일 단계에서 잡는다.
- 대안: Leptos (Rust 풀스택, 학습 부담 큼), htmx + 서버 템플릿

### D-09. mock 업체를 Rust로 직접 구현, 성격이 다른 두 곳(alpha, beta)
- 상태: 채택
- 이유: 장애를 마음대로 주입하기 위해서다. 두 업체의 API 스타일 차이 때문에 어댑터 패턴이 실제로 필요해진다.

### D-10. 서킷·rate limit 상태는 프로세스 메모리에 (단일 인스턴스 전제)
- 상태: 채택
- 이유: 단순하다.
- 결과: 인스턴스를 여러 개 띄우면 업체 한도를 인스턴스 수만큼 넘길 수 있다. 확장할 때 Redis로 옮긴다.

### D-11. 식별자는 UUID v7, 시각은 UTC `timestamptz`
- 상태: 채택
- 이유: UUID v7은 시간순으로 정렬되어 B-tree 인덱스에 유리하고, 업체에 보내는 참조값으로도 그대로 쓸 수 있다.

### D-12. 에러 응답은 RFC 9457 Problem Details
- 상태: 채택
- 이유: 표준 형식이다. 클라이언트가 `type`으로 에러 종류를 구분할 수 있다.

### D-13. 품질 게이트는 단일 스크립트 + 고정 toolchain, 세 곳(로컬 · Claude Code 훅 · CI)에서 같은 명령 실행
- 상태: 채택
- 이유: 로컬에서 통과한 코드가 CI에서 깨지는 일을 없앤다. Claude Code가 응답을 끝내기 전에 Stop 훅이 fmt + clippy를 돌려, 실패하면 Claude가 스스로 고치게 한다. 코딩 규칙(`unwrap` · `println!` 금지 등)은 문서로만 두지 않고 clippy 린트로 강제한다.
- 구성: `scripts/check.sh`(fmt · clippy · nextest · cargo-deny · cargo-shear), `rust-toolchain.toml`(1.99.1 고정), `[workspace.lints]`, `.claude/settings.json` 훅, `.github/workflows/ci.yml`, Dependabot
- 결과: toolchain과 CI 도구 버전은 사람이 직접 올린다. Stop 훅 때문에 응답이 끝날 때마다 수 초가 더 걸린다 (Rust 변경이 없으면 건너뜀).
- 대안: pre-commit 훅 (커밋할 때만 돌아서 Claude의 작업 중간 피드백이 늦음), `just`/`make` (도구 추가 설치 필요)

### D-14. API 키는 256비트 무작위 값, DB에는 SHA-256 해시만 저장
- 상태: 채택
- 이유: 비밀번호는 사람이 정해 짧고 추측하기 쉬워서 느린 해시(argon2 등)가 필요하다. API 키는 서버가 만든 256비트 무작위 값이라, 빠른 해시로도 역산하거나 추측할 수 없다. 빠른 해시라서 요청마다 인증해도 비용이 거의 없고, 해시 값으로 바로 DB를 조회할 수 있다.
- 결과: 키 원문은 발급할 때 한 번만 보여 준다(`booking-server create-client`). 잃어버리면 새로 발급한다.
- 대안: argon2 (요청마다 수십 ms가 들고, 해시로 바로 조회할 수 없어 키에 별도 식별자가 필요함)

### D-15. sqlx 쿼리 검사는 커밋된 오프라인 데이터(`.sqlx/`)로, 최신 여부는 별도 단계로 확인
- 상태: 채택
- 이유: CI와 Stop 훅에서 DB 없이 컴파일할 수 있다. SQL을 바꾸고 `cargo sqlx prepare`를 잊으면 바로 컴파일 에러가 난다.
- 구성: `scripts/check.sh`가 `SQLX_OFFLINE=true`를 기본으로 쓴다. `sqlx` 단계는 마이그레이션을 적용한 뒤 `cargo sqlx prepare --check`로 `.sqlx/`가 스키마와 맞는지 확인한다. CI의 `test` · `sqlx` job에는 PostgreSQL 서비스 컨테이너를 붙인다.
- 결과: 쿼리를 바꾸면 `cargo sqlx prepare --workspace -- --all-targets`를 돌려 `.sqlx/`를 함께 커밋해야 한다. sqlx CLI 버전은 sqlx 크레이트(0.9.0)와 맞춘다.

### D-16. 업체 어댑터는 `async-trait`으로 만든 `dyn` trait
- 상태: 채택
- 이유: 업체마다 다른 어댑터를 실행 중에 업체 ID로 골라 써야 해서 `Arc<dyn ProviderAdapter>`가 필요하다. Rust 1.99에서도 `async fn`이 있는 trait은 `dyn`으로 쓸 수 없어서(E0038) `async-trait` 크레이트로 해결한다. 호출마다 Future를 힙에 하나 할당하지만, 네트워크 호출에 비하면 무시할 만하다.
- 대안: enum으로 어댑터를 나열해 `match`로 고르기 (업체를 추가할 때마다 enum과 모든 메서드를 고쳐야 하고, 테스트용 가짜 어댑터를 넣기 어려움)

### D-17. 어댑터는 결과와 함께 응답 메타를 돌려준다
- 상태: 채택
- 이유: `provider_calls`에 HTTP 상태와 `X-RateLimit-Remaining`을 남겨야 하는데, 결과 타입(`Result<T, ProviderError>`)만으로는 성공 응답의 상태 코드나 헤더를 알 수 없다. 그래서 어댑터 메서드는 `AdapterReply<T> { result, meta }`를 돌려준다. 어댑터 안에서는 `?`를 쓰는 내부 함수가 `&mut ResponseMeta`를 채우고, trait 메서드가 둘을 묶는다. `ResilientProvider`가 메타는 기록에 쓰고 결과만 바깥에 넘기므로 워커는 이 구조를 모른다.
- 대안: `ProviderError`에 상태 코드를 넣기 (성공 응답의 정보는 담을 수 없음), 어댑터가 마지막으로 본 값을 공유 변수에 두기 (동시 호출끼리 값이 섞임)

### D-18. 호출 기록은 providers가 채널로 보내고, DB 저장은 booking-server가 맡는다
- 상태: 채택
- 이유: providers가 db 크레이트에 의존하지 않게 해서 크레이트 의존 방향을 지킨다 ([01 §6](01-architecture.md#6-저장소-구조-cargo-workspace)). 보내는 쪽은 `try_send`만 하므로 업체 호출 경로가 DB를 기다리지 않는다.
- 결과: 기록 태스크는 보내는 쪽(`CallRecorder`)이 모두 사라질 때 끝난다. 서버가 멈출 때 워커가 먼저 끝나야 남은 기록이 저장된다. 기록이 필요 없는 테스트는 `CallRecorder::discard()`를 쓴다.
- 대안: providers → db 의존 (방향 위반), 기록용 trait 객체 (구현이 하나뿐이라 지금은 채널로 충분)

### D-19. 워커의 업체별 동시 처리 제한은 두 번째 업체(Phase 6)와 함께 만든다
- 상태: 채택 ([03 §8](03-resilience.md#8-bulkhead-격리)의 일부를 뒤로 미룸)
- 이유: 지금은 업체가 alpha 하나뿐이라 업체 사이의 격리를 만들어도 시험할 수 없다. 업체 쪽 bulkhead(Semaphore)는 Phase 3에서 구현해서, 한 업체에 대한 동시 호출 수는 이미 제한된다.
- 결과: Phase 6 전까지는 한 업체가 느려지면 워커 루프(기본 4개)가 모두 그 업체 호출에 묶일 수 있다 (호출 예산 15초가 상한). 서킷이 열리면 호출이 바로 끝나므로 풀린다. "한 업체만 장애" 시나리오 테스트도 Phase 6에서 추가한다.
