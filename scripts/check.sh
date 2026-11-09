#!/usr/bin/env bash
# 품질 검사 단일 진입점. 로컬 개발, Claude Code Stop 훅, GitHub Actions CI가 모두 이 스크립트를 쓴다.
# 검사 내용을 바꿀 때는 이 파일 한 곳만 고치면 세 곳에 동시에 반영된다.
#
# 사용법:
#   scripts/check.sh            전체 검사 (all)
#   scripts/check.sh fast       포맷 + clippy (Stop 훅이 쓰는 빠른 검사)
#   scripts/check.sh <단계>...  fmt | clippy | test | deny | shear 중 골라서 실행
set -euo pipefail

cd "$(dirname "$0")/.."

# 훅이나 GUI에서 실행되면 셸 프로필을 읽지 않아 cargo가 PATH에 없을 수 있다.
if [[ -d "$HOME/.cargo/bin" && ":$PATH:" != *":$HOME/.cargo/bin:"* ]]; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi

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
  step "test: 단위 · 통합 테스트 (nextest)"
  cargo nextest run --workspace --all-features --locked --no-tests=pass
  step "test: 문서 테스트"
  cargo test --workspace --all-features --locked --doc
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
    all)    run_fmt; run_clippy; run_test; run_deny; run_shear ;;
    fmt)    run_fmt ;;
    clippy) run_clippy ;;
    test)   run_test ;;
    deny)   run_deny ;;
    shear)  run_shear ;;
    *) echo "알 수 없는 단계: $s (fast | all | fmt | clippy | test | deny | shear)" >&2; exit 2 ;;
  esac
done

printf '\n%s✔ 검사 통과: %s%s\n' "$bold" "${steps[*]}" "$reset"
