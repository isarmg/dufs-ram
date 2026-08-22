#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"

required_version="cargo-llvm-cov 0.8.6"
actual_version="$(cargo llvm-cov --version 2>/dev/null)" || {
  printf 'required Cargo subcommand is unavailable: cargo llvm-cov\n' >&2
  exit 1
}
[[ "$actual_version" == "$required_version" ]] || {
  printf 'required %s; found: %s\n' "$required_version" "$actual_version" >&2
  exit 1
}

cargo llvm-cov \
  --locked \
  --all-targets \
  --all-features \
  --summary-only \
  --fail-under-lines 70
