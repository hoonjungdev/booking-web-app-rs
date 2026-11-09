#!/usr/bin/env bash
# Stop 훅: Claude가 응답을 끝내려 할 때 빠른 품질 검사(scripts/check.sh fast = fmt + clippy)를 돌린다.
#  - 실패하면 exit 2 + stderr로 오류를 Claude에게 돌려보내 마저 고치게 한다.
#  - 이미 한 번 돌려보낸 뒤(stop_hook_active)에도 실패하면 더 막지 않고 사용자에게 경고만 띄운다 (무한 반복 방지).
#  - Rust 관련 변경이 없으면 검사를 건너뛴다.
set -uo pipefail

input=$(cat)
project="${CLAUDE_PROJECT_DIR:-$(pwd)}"
cd "$project" || exit 0

# 커밋되지 않은 Rust 관련 변경(새 파일 포함)이 있을 때만 검사한다.
if ! git status --porcelain --untracked-files=all -- '*.rs' '*.toml' 'Cargo.lock' 2>/dev/null | grep -q .; then
  exit 0
fi

if output=$(scripts/check.sh fast 2>&1); then
  exit 0
fi

summary=$(printf '%s\n' "$output" | grep -vE '^\s+(Compiling|Checking|Finished|Blocking|Locking)' | tail -n 60)

if [[ "$(jq -r '.stop_hook_active // false' <<<"$input")" == "true" ]]; then
  jq -n --arg msg "⚠ 품질 검사(scripts/check.sh fast)가 아직 실패 상태입니다. 직접 확인하거나 Claude에게 수정을 요청하세요." \
    '{systemMessage: $msg}'
  exit 0
fi

{
  echo "품질 검사(scripts/check.sh fast)가 실패했습니다. 아래 오류를 고친 뒤 마무리하세요."
  echo "포맷 문제는 'cargo fmt --all'로 고칠 수 있습니다. 린트를 끄는 #[allow]는 이유가 분명할 때만 쓰고 주석으로 이유를 남기세요."
  echo
  printf '%s\n' "$summary"
} >&2
exit 2
