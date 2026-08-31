#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"

required_version="cargo-llvm-cov 0.8.6"
minimum_total_line_coverage=70
# Keep every instrumented source file above zero coverage so a well-covered
# test suite cannot hide a newly unexercised security- or protocol-critical
# module behind the repository-wide aggregate.
minimum_file_line_coverage=1
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
  --target x86_64-unknown-linux-gnu \
  --all-targets \
  --all-features \
  --summary-only \
  --fail-under-lines "$minimum_total_line_coverage" \
  --fail-under-file-lines "$minimum_file_line_coverage"
