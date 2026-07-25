#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"

run() {
  printf '\n==> %s\n' "$*"
  "$@"
}

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf 'required command is unavailable: %s\n' "$1" >&2
    exit 1
  fi
}

require cargo
require git
require node
require npm

run rustc --version
run cargo --version
run node --version
run npm --version

run cargo fmt --all --check
run cargo clippy --locked --all-targets --all-features -- -D warnings
run cargo test --locked --all-targets --all-features

if ! cargo audit --version >/dev/null 2>&1; then
  printf 'required Cargo subcommand is unavailable: cargo audit\n' >&2
  exit 1
fi
run cargo audit

run npm ci
run npm run test:frontend
if command -v microsoft-edge >/dev/null 2>&1 || command -v microsoft-edge-stable >/dev/null 2>&1; then
  run npm run test:frontend:edge
else
  printf '\n==> SKIP: 未安装 Microsoft Edge；Chromium 与 Firefox 已作为必需矩阵执行。\n'
fi
run npm audit --audit-level=high

run git diff --check
run git diff --cached --check
if [[ -n "$(git status --porcelain)" ]]; then
  printf 'working tree is not clean:\n' >&2
  git status --short >&2
  exit 1
fi
printf '\n==> all required checks passed; working tree is clean\n'
