#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"
required_cargo_audit_version="0.22.2"

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
require nginx
require systemd-analyze

shell_scripts=(
  scripts/check.sh
  scripts/check-coverage.sh
  scripts/check-deployment.sh
  scripts/package-release.sh
  tests/data/generate_tls_certs.sh
)

run rustc --version
run cargo --version
run node --version
run npm --version
run bash -n "${shell_scripts[@]}"
if command -v shellcheck >/dev/null 2>&1; then
  run shellcheck --version
  if [[ "${DUFS_REQUIRE_SHELLCHECK:-}" == "1" ]]; then
    shellcheck_version="$(
      shellcheck --version | while IFS= read -r line; do
        if [[ "$line" == "version: "* ]]; then
          printf '%s\n' "${line#version: }"
          break
        fi
      done
    )"
    if [[ "$shellcheck_version" != "0.11.0" ]]; then
      printf 'ShellCheck 0.11.0 is required; found %s\n' \
        "${shellcheck_version:-unknown}" >&2
      exit 1
    fi
  fi
  run shellcheck --severity=warning "${shell_scripts[@]}"
elif [[ "${DUFS_REQUIRE_SHELLCHECK:-}" == "1" ]]; then
  printf 'required command is unavailable: shellcheck\n' >&2
  exit 1
else
  printf '\n==> SKIP: 未安装 ShellCheck；CI 会固定使用 0.11.0 并强制执行。\n'
fi

cargo_audit_version="$(cargo audit --version 2>/dev/null)" || {
  printf 'required Cargo subcommand is unavailable: cargo audit\n' >&2
  exit 1
}
expected_cargo_audit_version="cargo-audit-audit $required_cargo_audit_version"
[[ "$cargo_audit_version" == "$expected_cargo_audit_version" ]] || {
  printf 'cargo-audit %s is required; found: %s\n' \
    "$required_cargo_audit_version" \
    "$cargo_audit_version" >&2
  exit 1
}
printf '\n==> %s\n' "$cargo_audit_version"
if [[ "${DUFS_ISOLATED_QUALITY_GATE:-}" == "1" ]]; then
  [[ -n "${DUFS_QUALITY_AUDIT_DB:-}" ]] || {
    printf 'isolated release gate requires its sealed RustSec database\n' >&2
    exit 1
  }
  run cargo audit \
    --db "$DUFS_QUALITY_AUDIT_DB" \
    --no-fetch \
    --no-yanked
elif [[ -n "${DUFS_QUALITY_AUDIT_DB:-}" ]]; then
  printf 'DUFS_QUALITY_AUDIT_DB is reserved for the isolated release gate\n' >&2
  exit 1
else
  run cargo audit --deny yanked
fi

# 发布自测统一聚合 normalize-sbom、third-party notices 与 npm cache seed，
# 避免在总检查入口重复执行或让三者的验证范围发生漂移。
run ./scripts/package-release.sh --self-test
run ./scripts/check-deployment.sh

run cargo fmt --all --check
run cargo clippy --locked --all-targets --all-features -- -D warnings
run cargo test --locked --all-targets --all-features
run ./scripts/check-coverage.sh

run npm ci --ignore-scripts --no-audit --no-fund
run ./node_modules/.bin/tsc --version
run node scripts/extract-release-notes.mjs --self-test
run node scripts/check-release-workflow.mjs
run npm run check:js
run npm run check:types
run npm run check:docs
run npm run test:frontend:unit
run npm run test:frontend
if command -v microsoft-edge >/dev/null 2>&1 || command -v microsoft-edge-stable >/dev/null 2>&1; then
  run npm run test:frontend:edge
else
  printf '\n==> SKIP: 未安装 Microsoft Edge；Chromium 与 Firefox 已作为必需矩阵执行。\n'
fi
run npm audit --audit-level=high

if [[ "${DUFS_ISOLATED_QUALITY_GATE:-}" == "1" ]]; then
  [[ "${DUFS_BUILD_GIT_SHA:-}" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || {
    printf 'isolated quality gate requires the verified full source commit\n' >&2
    exit 1
  }
  [[ ! -e .git && ! -L .git ]] || {
    printf 'isolated quality source unexpectedly contains Git metadata\n' >&2
    exit 1
  }
  printf \
    '\n==> isolated checks passed; the release packager will re-verify source tree %s\n' \
    "$DUFS_BUILD_GIT_SHA"
else
  run git diff --check
  run git diff --cached --check
  if [[ -n "$(git status --porcelain)" ]]; then
    printf 'working tree is not clean:\n' >&2
    git status --short >&2
    exit 1
  fi
  printf '\n==> all required checks passed; working tree is clean\n'
fi
