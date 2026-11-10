#!/usr/bin/env bash
# 품질 검사 단일 진입점. 로컬 개발, Claude Code Stop 훅, GitHub Actions CI가 모두 이 스크립트를 쓴다.
# 검사 내용을 바꿀 때는 이 파일 한 곳만 고치면 세 곳에 동시에 반영된다.
#
# 사용법:
#   scripts/check.sh            전체 검사 (all)
#   scripts/check.sh fast       포맷 + clippy (Stop 훅이 쓰는 빠른 검사)
#   scripts/check.sh <단계>...  fmt | clippy | test | sqlx | deny | shear 중 골라서 실행
#
# DB가 필요한 단계: test(테스트마다 임시 DB를 만든다), sqlx. 먼저 `docker compose up -d --wait`.
set -euo pipefail

cd "$(dirname "$0")/.."

# 훅이나 GUI에서 실행되면 셸 프로필을 읽지 않아 cargo가 PATH에 없을 수 있다.
if [[ -d "$HOME/.cargo/bin" && ":$PATH:" != *":$HOME/.cargo/bin:"* ]]; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi

# 로컬 docker compose의 PostgreSQL이 기본값. CI는 환경 변수로 넘긴다.
export DATABASE_URL="${DATABASE_URL:-postgres://booking:booking@127.0.0.1:5432/booking}"
# sqlx 매크로는 커밋된 오프라인 데이터(.sqlx/)로 검사한다. 그래서 DB 없이도 컴파일되고,
# SQL을 바꾼 뒤 `cargo sqlx prepare`를 잊으면 바로 컴파일 에러로 드러난다.
export SQLX_OFFLINE="${SQLX_OFFLINE:-true}"

bold=$'\033[1m'; red=$'\033[31m'; reset=$'\033[0m'
[[ -t 1 ]] || { bold=""; red=""; reset=""; }

step() { printf '\n%s▶ %s%s\n' "$bold" "$*" "$reset"; }

require() {
  # $1: 실행 파일, $2: 설치 안내
  if ! command -v "$1" >/dev/null 2>&1; then
    printf '%s%s 가 설치되어 있지 않습니다.%s 설치: %s\n' "$red" "$1" "$reset" "$2" >&2
    exit 127
  fi
}

require_db() {
  # DATABASE_URL에서 호스트와 포트를 꺼내 TCP로 연결해 본다 (psql 없이 bash만으로)
  local hostport host port
  hostport=$(sed -E 's#^[a-z]+://([^@/]*@)?([^/?]+).*#\2#' <<<"$DATABASE_URL")
  host=${hostport%%:*}
  port=${hostport##*:}
  [[ "$port" == "$hostport" ]] && port=5432
  if ! (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null; then
    printf '%sDB(%s:%s)에 연결할 수 없습니다.%s 먼저 실행: docker compose up -d --wait\n' \
      "$red" "$host" "$port" "$reset" >&2
    exit 1
  fi
}

run_fmt() {
  step "fmt: 포맷 검사"
  cargo fmt --all -- --check
}

run_clippy() {
  step "clippy: 정적 분석 (경고도 실패로 처리)"
  cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
}

run_test() {
  require cargo-nextest "brew install cargo-nextest"
  require_db
  step "test: 단위 · 통합 테스트 (nextest)"
  # --no-fail-fast: 실패가 있어도 끝까지 돌려 실패를 한 번에 모두 보여 준다
  cargo nextest run --workspace --all-features --locked --no-tests=pass --no-fail-fast
  step "test: 문서 테스트"
  cargo test --workspace --all-features --locked --doc
}

run_sqlx() {
  require sqlx "brew install sqlx-cli"
  require_db
  step "sqlx: 마이그레이션 적용 + 오프라인 쿼리 데이터(.sqlx/)가 최신인지 검사"
  sqlx migrate run --source migrations
  # prepare는 실제 DB에 SQL을 보내 검사해야 하므로 이 명령에서만 온라인 모드로 돌린다
  SQLX_OFFLINE=false cargo sqlx prepare --workspace --check -- --all-targets
}

run_deny() {
  require cargo-deny "brew install cargo-deny"
  step "deny: 보안 권고 · 라이선스 · 중복 · 출처 검사"
  cargo deny --all-features check
}

run_shear() {
  require cargo-shear "brew install cargo-shear"
  step "shear: 쓰지 않는 의존성 검사"
  cargo shear
}

steps=("$@")
[[ ${#steps[@]} -eq 0 ]] && steps=(all)

for s in "${steps[@]}"; do
  case "$s" in
    fast)   run_fmt; run_clippy ;;
    all)    run_fmt; run_clippy; run_test; run_sqlx; run_deny; run_shear ;;
    fmt)    run_fmt ;;
    clippy) run_clippy ;;
    test)   run_test ;;
    sqlx)   run_sqlx ;;
    deny)   run_deny ;;
    shear)  run_shear ;;
    *) echo "알 수 없는 단계: $s (fast | all | fmt | clippy | test | sqlx | deny | shear)" >&2; exit 2 ;;
  esac
done

printf '\n%s✔ 검사 통과: %s%s\n' "$bold" "${steps[*]}" "$reset"
