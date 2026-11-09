#!/usr/bin/env bash
# PostToolUse(Edit|Write|MultiEdit) 훅: Claude가 수정한 .rs 파일을 곧바로 rustfmt로 정리한다.
# 문법 오류 등으로 rustfmt가 실패해도 막지 않는다 (그런 오류는 Stop 훅의 clippy가 잡는다).
set -uo pipefail

project="${CLAUDE_PROJECT_DIR:-$(pwd)}"
export PATH="$HOME/.cargo/bin:$PATH"

file=$(jq -r '.tool_response.filePath // .tool_input.file_path // empty')

# 프로젝트 안의 .rs 파일만 대상으로 한다.
[[ "$file" == *.rs && -f "$file" && "$file" == "$project"/* ]] || exit 0

# rust-toolchain.toml이 적용되도록 프로젝트 디렉터리에서 실행한다.
# rustfmt.toml은 파일 위치에서 위로 올라가며 찾으므로 자동으로 적용된다.
cd "$project" || exit 0
rustfmt --quiet "$file" >/dev/null 2>&1 || true
exit 0
