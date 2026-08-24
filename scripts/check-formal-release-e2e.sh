#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
test_root=""

cleanup() {
  local status=$?
  local cleanup_failed=false

  trap - EXIT HUP INT TERM
  if [[ -n "$test_root" ]]; then
    case "${test_root##*/}" in
      dufs-formal-release-e2e.*)
        if ! rm -rf --one-file-system -- "$test_root"; then
          printf 'Unable to remove formal release E2E directory: %s\n' \
            "$test_root" >&2
          cleanup_failed=true
        fi
        ;;
      *)
        printf 'Refusing to remove unexpected E2E path: %s\n' \
          "$test_root" >&2
        cleanup_failed=true
        ;;
    esac
  fi
  if [[ "$cleanup_failed" == true && "$status" -eq 0 ]]; then
    status=1
  fi
  exit "$status"
}

trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

for command_name in \
  chmod \
  cmp \
  env \
  find \
  git \
  grep \
  install \
  mktemp \
  openssl \
  rm \
  rustc \
  sed \
  sha256sum \
  stat \
  tar \
  wc
do
  command -v "$command_name" >/dev/null 2>&1 || {
    printf 'required command is unavailable: %s\n' "$command_name" >&2
    exit 1
  }
done

source_sha="$(git -C "$project_dir" rev-parse --verify 'HEAD^{commit}')"
[[ "$source_sha" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || {
  printf 'Unable to resolve a full source commit for the release E2E.\n' >&2
  exit 1
}
[[ -z "$(
  git -C "$project_dir" status --porcelain --untracked-files=all
)" ]] || {
  printf 'Formal release E2E requires a clean source checkout.\n' >&2
  exit 1
}

temporary_parent="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
[[ -d "$temporary_parent" && ! -L "$temporary_parent" ]] || {
  printf 'Formal release E2E temporary parent is unavailable: %s\n' \
    "$temporary_parent" >&2
  exit 1
}
test_root="$(mktemp -d --tmpdir="$temporary_parent" \
  dufs-formal-release-e2e.XXXXXXXX)"
chmod 0700 "$test_root"

# Exercise path serialization as part of the real release invocation. The
# source checkout retains a backslash, while the supported output path covers
# shell and replacement-language metacharacters without interpreting them.
# package-release.sh self-tests the unsupported output-backslash rejection.
repository="$test_root/checkout with spaces & # \\ source"
output_dir="$test_root/output with spaces & # release"
signing_key="$test_root/ephemeral Ed25519 signing key.pem"
unpack_dir="$test_root/verified unpack"
expected_public_key="$test_root/expected public key.pem"

git clone --quiet --no-checkout --no-local -- "$project_dir" "$repository"
git -C "$repository" checkout --quiet --detach "$source_sha"
[[ "$(git -C "$repository" rev-parse --verify 'HEAD^{commit}')" == \
  "$source_sha" ]] || {
  printf 'Isolated release checkout did not preserve the source commit.\n' >&2
  exit 1
}

mapfile -t versions < <(
  sed -n \
    '/^\[package\]$/,/^\[/s/^version = "\([^"]*\)"/\1/p' \
    "$repository/Cargo.toml"
)
if ((${#versions[@]} != 1)); then
  printf 'Expected one package version; found %s.\n' \
    "${#versions[@]}" >&2
  exit 1
fi
version="${versions[0]}"
if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$ ]]; then
  printf 'Cargo package version is not a safe semantic version: %s\n' \
    "$version" >&2
  exit 1
fi
release_tag="v$version"
git -C "$repository" \
  -c user.name=formal-release-e2e \
  -c user.email=formal-release-e2e@example.invalid \
  -c tag.gpgSign=false \
  tag --annotate --force --message='formal release E2E' \
    "$release_tag" "$source_sha"
[[ "$(
  git -C "$repository" rev-parse --verify "refs/tags/$release_tag^{commit}"
)" == "$source_sha" ]] || {
  printf 'E2E release tag does not resolve to the source commit.\n' >&2
  exit 1
}
[[ -z "$(
  git -C "$repository" status --porcelain --untracked-files=all
)" ]] || {
  printf 'Isolated release checkout became dirty before packaging.\n' >&2
  exit 1
}

install -d -m 0700 "$output_dir" "$unpack_dir"
openssl genpkey -algorithm Ed25519 -out "$signing_key"
chmod 0600 "$signing_key"

(
  cd "$repository"
  ./scripts/package-release.sh \
    --signing-key "$signing_key" \
    --output-dir "$output_dir"
)

toolchain="$(
  sed -n 's/^channel = "\([^"]*\)"/\1/p' \
    "$repository/rust-toolchain.toml"
)"
[[ -n "$toolchain" && "$toolchain" != *$'\n'* ]] || {
  printf 'Unable to determine the pinned Rust toolchain.\n' >&2
  exit 1
}
host_target="$(
  env RUSTUP_TOOLCHAIN="$toolchain" rustc -vV | sed -n 's/^host: //p'
)"
[[ -n "$host_target" && "$host_target" != *$'\n'* ]] || {
  printf 'Unable to determine the release host target.\n' >&2
  exit 1
}

release_name="dufs-${version}-${host_target}-${source_sha:0:12}"
release_dir="$output_dir/$release_name.release"
archive_name="$release_name.tar.gz"
checksum_name="$archive_name.sha256"
signature_name="$checksum_name.sig"
public_key_name="$checksum_name.pub.pem"
archive="$release_dir/$archive_name"
checksum="$release_dir/$checksum_name"
signature="$release_dir/$signature_name"
public_key="$release_dir/$public_key_name"

[[ -d "$release_dir" && ! -L "$release_dir" ]] || {
  printf 'Formal release directory is missing or unsafe: %s\n' \
    "$release_dir" >&2
  exit 1
}
entry_count="$(
  find -P "$output_dir" -mindepth 1 -maxdepth 2 -printf x | wc -c
)"
[[ "$entry_count" == "5" ]] || {
  printf 'Expected one release directory and four artifacts; found %s entries.\n' \
    "$entry_count" >&2
  exit 1
}
for artifact in "$archive" "$checksum" "$signature" "$public_key"; do
  if [[ ! -f "$artifact" || -L "$artifact" || \
    "$(stat -Lc '%h:%a' -- "$artifact")" != "1:644" ]]
  then
    printf 'Formal release artifact has an unsafe shape or mode: %s\n' \
      "$artifact" >&2
    exit 1
  fi
done

(
  cd "$release_dir"
  sha256sum --check "$checksum_name"
)
openssl pkey -in "$signing_key" -pubout -out "$expected_public_key"
cmp -- "$expected_public_key" "$public_key"
openssl pkeyutl \
  -verify \
  -rawin \
  -pubin \
  -inkey "$public_key" \
  -sigfile "$signature" \
  -in "$checksum"

tar \
  --extract \
  --gzip \
  --file="$archive" \
  --directory="$unpack_dir" \
  --no-same-owner
package_root="$unpack_dir/$release_name"
[[ -d "$package_root" && ! -L "$package_root" ]] || {
  printf 'Formal release archive did not contain the expected package root.\n' \
    >&2
  exit 1
}
unpacked_entry_count="$(
  find -P "$unpack_dir" -mindepth 1 -maxdepth 1 -printf x | wc -c
)"
[[ "$unpacked_entry_count" == "1" ]] || {
  printf 'Formal release archive contained unexpected top-level entries.\n' >&2
  exit 1
}
(
  cd "$package_root"
  sha256sum --check SHA256SUMS
)
expected_version="dufs $version (git $source_sha)"
actual_version="$("$package_root/dufs" --version)"
[[ "$actual_version" == "$expected_version" ]] || {
  printf 'Expected packaged version %s; found %s.\n' \
    "$expected_version" "$actual_version" >&2
  exit 1
}
grep -Fxq "source_sha=$source_sha" \
  "$package_root/BUILD-ENVIRONMENT.txt"
grep -Fxq "source_version=$version" \
  "$package_root/BUILD-ENVIRONMENT.txt"

printf 'formal signed release package E2E passed for %s at %s\n' \
  "$release_tag" "$source_sha"
