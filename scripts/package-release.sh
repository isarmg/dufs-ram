#!/usr/bin/env bash
set -euo pipefail

invocation_dir="$(pwd -P)"
script_source="${BASH_SOURCE[0]}"
script_parent="${script_source%/*}"
if [[ "$script_parent" == "$script_source" ]]; then
  script_parent="."
fi
project_dir="$(cd -P -- "$script_parent/.." && pwd -P)"
cd "$project_dir"
packager_pid="$BASHPID"
[[ "$packager_pid" =~ ^[1-9][0-9]*$ ]] || {
  printf 'Unable to determine the release packager process ID.\n' >&2
  exit 1
}

usage() {
  printf 'Usage: %s --signing-key <PEM private key> [--output-dir <directory>]\n' "$0"
  printf '       %s --self-test\n' "$0"
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || {
    printf 'Required command is unavailable: %s\n' "$1" >&2
    exit 1
  }
}

run_node_entrypoint() {
  local node_command="$1"
  local entrypoint="$2"
  shift 2

  # Staged release paths are anchored below the packager process directory fd.
  # Node's default main-module realpath would expose the physical output name
  # again and, on POSIX, rejects a valid backslash in that name as an encoded
  # URL separator.
  "$node_command" --preserve-symlinks-main "$entrypoint" "$@"
}

require_shellcheck_version() {
  local output
  local line
  local version=""

  require_command shellcheck
  output="$(LC_ALL=C shellcheck --version)" || {
    printf 'Unable to determine ShellCheck version.\n' >&2
    exit 1
  }
  while IFS= read -r line; do
    if [[ "$line" == "version: "* ]]; then
      version="${line#version: }"
      break
    fi
  done <<< "$output"
  if [[ "$version" != "0.11.0" ]]; then
    printf 'ShellCheck 0.11.0 is required; found %s\n' \
      "${version:-unknown}" >&2
    exit 1
  fi
}

capture_version_line() {
  local label="$1"
  shift
  local output

  output="$(LC_ALL=C "$@")" || {
    printf 'Unable to determine %s version.\n' "$label" >&2
    return 1
  }
  output="${output%%$'\n'*}"
  [[ -n "$output" && "$output" != *$'\r'* && "$output" != *$'\x1f'* ]] || {
    printf 'Invalid %s version output.\n' "$label" >&2
    return 1
  }
  printf '%s\n' "$output"
}

validate_cargo_audit_version_line() {
  local actual="$1"
  local required_version="$2"

  [[ "$actual" == "cargo-audit-audit $required_version" ]]
}

validate_advisory_database_freshness() {
  local fetch_epoch="$1"
  local current_epoch="$2"
  local maximum_age_seconds="$3"
  local maximum_future_skew_seconds="$4"
  local fetch_number
  local current_number
  local maximum_age_number
  local maximum_future_skew_number

  [[ "$fetch_epoch" =~ ^[0-9]{1,12}$ &&
    "$current_epoch" =~ ^[0-9]{1,12}$ &&
    "$maximum_age_seconds" =~ ^[0-9]{1,12}$ &&
    "$maximum_future_skew_seconds" =~ ^[0-9]{1,12}$ ]] || return 2
  fetch_number=$((10#$fetch_epoch))
  current_number=$((10#$current_epoch))
  maximum_age_number=$((10#$maximum_age_seconds))
  maximum_future_skew_number=$((10#$maximum_future_skew_seconds))

  (( fetch_number <= current_number + maximum_future_skew_number )) || return 2
  if (( current_number > fetch_number &&
    current_number - fetch_number > maximum_age_number ))
  then
    return 1
  fi
}

advisory_database_revision() {
  local database="$1"
  local revision

  [[ -d "$database" && ! -L "$database" &&
    -d "$database/.git" && ! -L "$database/.git" ]] || {
    printf 'RustSec advisory database is not a physical Git worktree: %s\n' \
      "$database" >&2
    return 1
  }
  revision="$(
    run_git_isolated -C "$database" rev-parse --verify 'HEAD^{commit}'
  )" || {
    printf 'Unable to resolve the RustSec advisory database revision.\n' >&2
    return 1
  }
  [[ "$revision" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || {
    printf 'RustSec advisory database returned an invalid revision.\n' >&2
    return 1
  }
  printf '%s\n' "$revision"
}

read_advisory_database_identity() {
  local database="$1"
  local revision
  local origin
  local fetch_head
  local fetched_revision
  local fetch_epoch

  revision="$(advisory_database_revision "$database")" || return $?
  origin="$(
    run_git_isolated -C "$database" remote get-url --all origin
  )" || {
    printf 'Unable to read the RustSec advisory database origin.\n' >&2
    return 1
  }
  case "$origin" in
    https://github.com/RustSec/advisory-db|https://github.com/RustSec/advisory-db.git) ;;
    *)
      printf 'RustSec advisory database has an untrusted origin: %s\n' \
        "${origin:-missing}" >&2
      return 1
      ;;
  esac

  fetch_head="$database/.git/FETCH_HEAD"
  [[ -f "$fetch_head" && ! -L "$fetch_head" ]] || {
    printf 'RustSec advisory database lacks a physical FETCH_HEAD freshness record.\n' >&2
    return 1
  }
  IFS=$'\t' read -r fetched_revision _ < "$fetch_head" || {
    printf 'Unable to read the RustSec advisory database FETCH_HEAD.\n' >&2
    return 1
  }
  [[ "$fetched_revision" == "$revision" ]] || {
    printf 'RustSec advisory database HEAD does not match its last fetched revision.\n' >&2
    return 1
  }
  fetch_epoch="$(stat -c '%Y' -- "$fetch_head")" || return $?
  [[ "$fetch_epoch" =~ ^[0-9]{1,12}$ ]] || {
    printf 'RustSec advisory database returned an invalid fetch timestamp.\n' >&2
    return 1
  }
  printf '%s %s\n' "$revision" "$fetch_epoch"
}

classify_advisory_database_identity() {
  local database="$1"
  local identity

  if identity="$(read_advisory_database_identity "$database")"; then
    printf 'reusable %s\n' "$identity"
  else
    printf 'unavailable\n'
  fi
}

run_advisory_database_git() {
  local database="$1"
  shift

  run_git_isolated \
    -c core.attributesFile=/dev/null \
    -c core.excludesFile=/dev/null \
    -c core.fileMode=true \
    -c core.fsmonitor=false \
    -c core.untrackedCache=false \
    -C "$database" \
    "$@"
}

run_advisory_database_git_with_index() {
  local database="$1"
  local index_file="$2"
  shift 2

  env -i \
    HOME=/ \
    XDG_CONFIG_HOME=/ \
    LANG=C \
    LC_ALL=C \
    PATH=/usr/bin:/bin \
    GIT_ATTR_NOSYSTEM=1 \
    GIT_CONFIG_GLOBAL=/dev/null \
    GIT_CONFIG_NOSYSTEM=1 \
    GIT_CONFIG_SYSTEM=/dev/null \
    GIT_INDEX_FILE="$index_file" \
    GIT_NO_REPLACE_OBJECTS=1 \
    GIT_OPTIONAL_LOCKS=0 \
    "$git_command" \
    -c core.attributesFile=/dev/null \
    -c core.excludesFile=/dev/null \
    -c core.fileMode=true \
    -c core.fsmonitor=false \
    -c core.untrackedCache=false \
    -C "$database" \
    "$@"
}

cleanup_advisory_database_validation() {
  local status=$?
  local cleanup_failed=false
  local index_path="${advisory_validation_index-}"
  local index_directory="${advisory_validation_directory-}"
  local cleanup_entry

  trap - EXIT HUP INT TERM
  set +e
  for cleanup_entry in "$index_path" "$index_path.lock"; do
    [[ -n "$index_path" ]] || continue
    if [[ "${cleanup_entry%/*}" != "$index_directory" ||
      "${cleanup_entry##*/}" != dufs-rustsec-index.* ]]
    then
      printf 'Refusing to remove unexpected RustSec verification index: %s\n' \
        "$cleanup_entry" >&2
      cleanup_failed=true
    elif [[ -e "$cleanup_entry" || -L "$cleanup_entry" ]] &&
      ! rm -f -- "$cleanup_entry"
    then
      cleanup_failed=true
    fi
  done
  if [[ "$cleanup_failed" == true && "$status" -eq 0 ]]; then
    status=1
  fi
  exit "$status"
}

reject_advisory_database_untracked_stream() {
  local stream_fd="$1"
  local untracked_path=""

  if IFS= read -r -d '' -u "$stream_fd" untracked_path; then
    printf 'RustSec advisory database contains an untracked path: %q\n' \
      "$untracked_path" >&2
    return 1
  fi
  [[ -z "$untracked_path" ]] || {
    printf 'RustSec untracked-path listing ended with a truncated entry: %q\n' \
      "$untracked_path" >&2
    return 1
  }
}

validate_advisory_database_tracked_stream() {
  local stream_fd="$1"
  local database="${advisory_validation_database-}"
  local entry=""
  local metadata
  local mode
  local object_type
  local expected_object_id
  local path
  local physical_path
  local file_metadata
  local raw_mode
  local link_count
  local numeric_mode
  local actual_object_id

  [[ -d "$database" && ! -L "$database" ]] || {
    printf 'RustSec tracked-file validator lacks a physical database.\n' >&2
    return 1
  }
  while IFS= read -r -d '' -u "$stream_fd" entry; do
    metadata="${entry%%$'\t'*}"
    path="${entry#*$'\t'}"
    read -r mode object_type expected_object_id <<< "$metadata"
    case "$mode:$object_type" in
      100644:blob|100755:blob) ;;
      120000:blob)
        printf 'Refusing symbolic link in RustSec database: %q\n' "$path" >&2
        return 1
        ;;
      160000:commit)
        printf 'Refusing submodule in RustSec database: %q\n' "$path" >&2
        return 1
        ;;
      *)
        printf \
          'Refusing unsupported RustSec Git entry mode/type %s/%s at %q.\n' \
          "$mode" \
          "$object_type" \
          "$path" >&2
        return 1
        ;;
    esac
    case "$path" in
      ""|/*|.|..|./*|../*|*/.|*/..|*/./*|*/../*)
        printf 'Refusing unsafe RustSec database path: %q\n' "$path" >&2
        return 1
        ;;
    esac
    [[ "$expected_object_id" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || {
      printf 'RustSec database entry has an invalid object ID: %q\n' \
        "$path" >&2
      return 1
    }

    physical_path="$database/$path"
    [[ -f "$physical_path" && ! -L "$physical_path" ]] || {
      printf 'RustSec tracked path is not a physical file: %q\n' "$path" >&2
      return 1
    }
    file_metadata="$(stat -c '%f %h' -- "$physical_path")" || return $?
    read -r raw_mode link_count <<< "$file_metadata"
    [[ "$raw_mode" =~ ^[0-9a-f]+$ && "$link_count" =~ ^[0-9]+$ ]] || {
      printf 'RustSec tracked path returned invalid metadata: %q\n' "$path" >&2
      return 1
    }
    numeric_mode=$((16#$raw_mode))
    (( (numeric_mode & 0170000) == 0100000 && link_count == 1 )) || {
      printf 'RustSec tracked path is not a private regular file: %q\n' \
        "$path" >&2
      return 1
    }
    case "$mode" in
      100644)
        (( (numeric_mode & 0111) == 0 )) || {
          printf 'RustSec tracked mode differs from revision at %q.\n' \
            "$path" >&2
          return 1
        }
        ;;
      100755)
        (( (numeric_mode & 0100) != 0 )) || {
          printf 'RustSec tracked mode differs from revision at %q.\n' \
            "$path" >&2
          return 1
        }
        ;;
    esac
    actual_object_id="$(
      run_advisory_database_git \
        "$database" \
        hash-object --no-filters -- "$path"
    )" || return $?
    [[ "$actual_object_id" == "$expected_object_id" ]] || {
      printf 'RustSec tracked content differs from revision at %q.\n' \
        "$path" >&2
      return 1
    }
  done
  [[ -z "$entry" ]] || {
    printf 'RustSec tracked-file listing ended with a truncated entry: %q\n' \
      "$entry" >&2
    return 1
  }
}

validate_advisory_database_state() (
  local database="$1"
  local expected_revision="${2:-}"
  local expected_fetch_epoch="${3:-}"
  local expected_index_checksum="${4:-}"
  local expected_config_checksum="${5:-}"
  local identity
  local revision
  local fetch_epoch
  local git_directory
  local git_common_directory
  local metadata_path
  local index_checksum
  local config_checksum
  local advisory_validation_directory
  local advisory_validation_index=""
  local advisory_validation_database="$database"

  trap cleanup_advisory_database_validation EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM

  validate_extracted_source_tree "$database" || return $?
  validate_source_git_metadata "$database" || return $?
  identity="$(read_advisory_database_identity "$database")" || return $?
  read -r revision fetch_epoch <<< "$identity"
  if [[ -n "$expected_revision" && "$revision" != "$expected_revision" ]]; then
    printf 'RustSec advisory database revision changed from %s to %s.\n' \
      "$expected_revision" \
      "$revision" >&2
    return 1
  fi
  if [[ -n "$expected_fetch_epoch" && \
    "$fetch_epoch" != "$expected_fetch_epoch" ]]
  then
    printf 'RustSec advisory database fetch epoch changed from %s to %s.\n' \
      "$expected_fetch_epoch" \
      "$fetch_epoch" >&2
    return 1
  fi

  git_directory="$(
    run_advisory_database_git "$database" \
      rev-parse --path-format=absolute --git-dir
  )" || return $?
  git_common_directory="$(
    run_advisory_database_git "$database" \
      rev-parse --path-format=absolute --git-common-dir
  )" || return $?
  for metadata_path in \
    "$git_directory/objects/info/alternates" \
    "$git_directory/objects/info/http-alternates" \
    "$git_common_directory/objects/info/alternates" \
    "$git_common_directory/objects/info/http-alternates"
  do
    if [[ -e "$metadata_path" || -L "$metadata_path" ]]; then
      printf 'RustSec advisory database contains object alternates: %s\n' \
        "$metadata_path" >&2
      return 1
    fi
  done
  [[ -f "$git_directory/index" && ! -L "$git_directory/index" ]] || {
    printf 'RustSec advisory database lacks a physical Git index.\n' >&2
    return 1
  }
  [[ -f "$git_common_directory/config" && \
    ! -L "$git_common_directory/config" ]] || {
    printf 'RustSec advisory database lacks a physical Git configuration.\n' >&2
    return 1
  }
  index_checksum="$(sha256sum < "$git_directory/index")"
  index_checksum="${index_checksum%% *}"
  config_checksum="$(sha256sum < "$git_common_directory/config")"
  config_checksum="${config_checksum%% *}"
  [[ "$index_checksum" =~ ^[0-9a-f]{64}$ && \
    "$config_checksum" =~ ^[0-9a-f]{64}$ ]] || {
    printf 'RustSec advisory database metadata checksums are invalid.\n' >&2
    return 1
  }
  if [[ -n "$expected_index_checksum" && \
    "$index_checksum" != "$expected_index_checksum" ]]
  then
    printf 'RustSec advisory database index changed after it was sealed.\n' >&2
    return 1
  fi
  if [[ -n "$expected_config_checksum" && \
    "$config_checksum" != "$expected_config_checksum" ]]
  then
    printf 'RustSec advisory database Git configuration changed after it was sealed.\n' \
      >&2
    return 1
  fi
  if ! run_advisory_database_git "$database" \
    diff-index --cached --quiet "$revision" --
  then
    printf 'RustSec advisory database index differs from revision %s.\n' \
      "$revision" >&2
    return 1
  fi
  with_private_nul_stream \
    validate_advisory_database_tracked_stream \
    run_advisory_database_git \
      "$database" \
      ls-tree -rz --full-tree "$revision" || return $?
  advisory_validation_directory="$({
    cd -P -- "${TMPDIR:-/tmp}" && pwd -P
  })" || {
    printf 'Unable to resolve the RustSec verification directory.\n' >&2
    return 1
  }
  advisory_validation_index="$(
    mktemp \
      --tmpdir="$advisory_validation_directory" \
      dufs-rustsec-index.XXXXXXXXXX
  )" || {
    printf 'Unable to create a private RustSec verification index.\n' >&2
    return 1
  }
  rm -f -- "$advisory_validation_index" || return $?
  run_advisory_database_git_with_index \
    "$database" \
    "$advisory_validation_index" \
    read-tree "$revision" || return $?
  with_private_nul_stream \
    reject_advisory_database_untracked_stream \
    run_advisory_database_git_with_index \
      "$database" \
      "$advisory_validation_index" \
      ls-files --others -z -- || return $?
  rm -f -- "$advisory_validation_index" "$advisory_validation_index.lock" || \
    return $?
  advisory_validation_index=""

  printf '%s %s %s %s\n' \
    "$revision" \
    "$fetch_epoch" \
    "$index_checksum" \
    "$config_checksum"
)

write_build_environment_manifest() {
  local destination="$1"
  local source_sha="$2"
  local source_version="$3"
  local source_date_epoch="$4"
  local target="$5"
  local rustc_version="$6"
  local cargo_version="$7"
  local cargo_cyclonedx_version="$8"
  local cargo_audit_version="$9"
  shift 9
  local rustsec_advisory_db_revision="$1"
  local rustsec_advisory_db_fetch_epoch="$2"
  local node_command="$3"
  local npm_command="$4"
  local node_version
  local npm_version
  local git_version
  local openssl_version
  local tar_version
  local gzip_version
  local mv_version
  local sha256sum_version

  node_version="$(capture_version_line Node "$node_command" --version)" || return $?
  npm_version="$(capture_version_line npm "$npm_command" --version)" || return $?
  git_version="$(capture_version_line Git "$git_command" --version)" || return $?
  openssl_version="$(capture_version_line OpenSSL openssl version)" || return $?
  tar_version="$(capture_version_line tar tar --version)" || return $?
  gzip_version="$(capture_version_line gzip gzip --version)" || return $?
  mv_version="$(capture_version_line mv mv --version)" || return $?
  sha256sum_version="$(capture_version_line sha256sum sha256sum --version)" || return $?

  {
    printf 'format=dufs-build-environment-v2\n'
    printf 'source_sha=%s\n' "$source_sha"
    printf 'source_version=%s\n' "$source_version"
    printf 'source_date_epoch=%s\n' "$source_date_epoch"
    printf 'target=%s\n' "$target"
    printf 'bash=%s\n' "$BASH_VERSION"
    printf 'rustc=%s\n' "$rustc_version"
    printf 'cargo=%s\n' "$cargo_version"
    printf 'cargo_cyclonedx=%s\n' "$cargo_cyclonedx_version"
    printf 'cargo_audit=%s\n' "$cargo_audit_version"
    printf 'rustsec_advisory_db_revision=%s\n' \
      "$rustsec_advisory_db_revision"
    printf 'rustsec_advisory_db_fetch_epoch=%s\n' \
      "$rustsec_advisory_db_fetch_epoch"
    printf 'node=%s\n' "$node_version"
    printf 'npm=%s\n' "$npm_version"
    printf 'git=%s\n' "$git_version"
    printf 'openssl=%s\n' "$openssl_version"
    printf 'tar=%s\n' "$tar_version"
    printf 'gzip=%s\n' "$gzip_version"
    printf 'mv=%s\n' "$mv_version"
    printf 'sha256sum=%s\n' "$sha256sum_version"
  } > "$destination"
  chmod 0644 "$destination"
}

resolve_invocation_path() {
  local path="$1"
  case "$path" in
    /*) printf '%s\n' "$path" ;;
    *) printf '%s/%s\n' "$invocation_dir" "$path" ;;
  esac
}

canonicalize_output_directory() {
  local requested_path="$1"
  local candidate

  case "$requested_path" in
    *$'\n'*|*$'\r'*|*$'\x1f'*)
      printf 'Output directory contains an unsupported control character.\n' >&2
      return 1
      ;;
  esac
  candidate="$(resolve_invocation_path "$requested_path")"
  mkdir -p -- "$candidate"
  (
    cd -P -- "$candidate"
    pwd -P
  )
}

validate_output_directory() {
  local directory="$1"
  local current_uid="$2"
  local owner
  local mode
  local numeric_mode

  case "$directory" in
    *\\*)
      printf 'Release output path contains an unsupported backslash: %s\n' \
        "$directory" >&2
      return 1
      ;;
  esac
  [[ -d "$directory" && ! -L "$directory" ]] || {
    printf 'Release output is not a physical directory: %s\n' "$directory" >&2
    return 1
  }
  read -r owner mode < <(stat -Lc '%u %a' -- "$directory")
  [[ "$owner" == "$current_uid" ]] || {
    printf 'Release output must be owned by uid %s: %s\n' \
      "$current_uid" \
      "$directory" >&2
    return 1
  }
  numeric_mode=$((8#$mode))
  (( (numeric_mode & 0022) == 0 )) || {
    printf 'Release output must not be group- or other-writable: %s (mode %s)\n' \
      "$directory" \
      "$mode" >&2
    return 1
  }
}

validate_private_directory_binding() {
  local anchored_directory="$1"
  local physical_directory="$2"
  local expected_metadata="$3"
  local description="$4"
  local anchored_metadata
  local physical_metadata
  local resolved_directory

  [[ -d "$physical_directory" && ! -L "$physical_directory" ]] || {
    printf '%s physical path is not a directory: %s\n' \
      "$description" \
      "$physical_directory" >&2
    return 1
  }
  resolved_directory="$(realpath -e -- "$anchored_directory")" || return $?
  [[ "$resolved_directory" == "$physical_directory" ]] || {
    printf '%s path binding changed unexpectedly.\n' "$description" >&2
    return 1
  }
  anchored_metadata="$(stat -Lc '%u:%a:%d:%i' -- "$anchored_directory")" || \
    return $?
  physical_metadata="$(stat -Lc '%u:%a:%d:%i' -- "$physical_directory")" || \
    return $?
  [[ "$anchored_metadata" == "$expected_metadata" && \
    "$physical_metadata" == "$expected_metadata" ]] || {
    printf '%s identity, owner, or mode changed unexpectedly.\n' \
      "$description" >&2
    return 1
  }
}

validate_public_output_binding() {
  local public_directory="$1"
  local locked_directory="$2"
  local expected_identity="$3"
  local public_identity
  local locked_identity

  public_identity="$(stat -Lc '%d:%i' -- "$public_directory")" || {
    printf 'Published output path is no longer accessible: %s\n' \
      "$public_directory" >&2
    return 1
  }
  locked_identity="$(stat -Lc '%d:%i' -- "$locked_directory")" || {
    printf 'Locked output directory descriptor is no longer accessible.\n' >&2
    return 1
  }
  [[ "$public_identity" == "$expected_identity" ]] || {
    printf 'Published output path was rebound to another directory: %s\n' \
      "$public_directory" >&2
    return 1
  }
  [[ "$locked_identity" == "$expected_identity" ]] || {
    printf 'Locked output directory identity changed unexpectedly.\n' >&2
    return 1
  }
}

validate_signing_key() {
  local key_path="$1"
  local current_uid="$2"
  local owner
  local mode
  local link_count
  local metadata
  local raw_mode
  local numeric_raw_mode

  metadata="$(stat -Lc '%u %a %h %f' -- "$key_path")" || {
    printf 'Signing key metadata is unavailable.\n' >&2
    return 1
  }
  read -r owner mode link_count raw_mode <<< "$metadata" || return $?
  numeric_raw_mode=$((16#$raw_mode))
  (( (numeric_raw_mode & 0170000) == 0100000 )) || {
    printf 'Signing key must be a regular file.\n' >&2
    return 1
  }
  [[ "$owner" == "$current_uid" ]] || {
    printf 'Signing key must be owned by uid %s.\n' "$current_uid" >&2
    return 1
  }
  [[ "$link_count" == "1" ]] || {
    printf 'Signing key must have exactly one hard link.\n' >&2
    return 1
  }
  case "$mode" in
    400|600) ;;
    *)
      printf 'Signing key mode must be 0400 or 0600; found %s.\n' \
        "$mode" >&2
      return 1
      ;;
  esac
}

run_git_isolated() {
  env -i \
    HOME=/ \
    XDG_CONFIG_HOME=/ \
    LANG=C \
    LC_ALL=C \
    PATH=/usr/bin:/bin \
    GIT_ATTR_NOSYSTEM=1 \
    GIT_CONFIG_GLOBAL=/dev/null \
    GIT_CONFIG_NOSYSTEM=1 \
    GIT_CONFIG_SYSTEM=/dev/null \
    GIT_NO_REPLACE_OBJECTS=1 \
    "$git_command" "$@"
}

run_source_git() {
  local repository="$1"
  shift

  run_git_isolated \
    -c core.attributesFile=/dev/null \
    -c core.excludesFile=/dev/null \
    -c core.fsmonitor=false \
    -c core.untrackedCache=false \
    -C "$repository" \
    "$@"
}

validate_source_git_metadata() {
  local repository="$1"
  local git_directory
  local git_common_directory
  local metadata_path
  local replace_refs
  local top_level

  top_level="$(run_source_git "$repository" rev-parse --show-toplevel)"
  [[ "$(realpath -e -- "$top_level")" == "$(realpath -e -- "$repository")" ]] || {
    printf 'Git worktree does not match the release source directory.\n' >&2
    return 1
  }
  git_directory="$(
    run_source_git "$repository" \
      rev-parse --path-format=absolute --git-dir
  )"
  git_common_directory="$(
    run_source_git "$repository" \
      rev-parse --path-format=absolute --git-common-dir
  )"

  for metadata_path in \
    "$git_directory/info/grafts" \
    "$git_directory/info/attributes" \
    "$git_common_directory/info/grafts" \
    "$git_common_directory/info/attributes"
  do
    if [[ -e "$metadata_path" || -L "$metadata_path" ]]; then
      printf 'Refusing repository-local Git metadata: %s\n' \
        "$metadata_path" >&2
      return 1
    fi
  done

  replace_refs="$(
    run_source_git "$repository" \
      for-each-ref --format='%(refname)' refs/replace/
  )"
  [[ -z "$replace_refs" ]] || {
    printf 'Refusing repository with refs/replace entries.\n' >&2
    return 1
  }
}

validate_release_source_state() {
  local repository="$1"
  local expected_commit="$2"
  local expected_tag="$3"
  local expected_version="$4"
  local current_commit
  local current_tag_commit
  local current_version
  local worktree_state

  validate_source_git_metadata "$repository" || return $?
  current_commit="$(
    run_source_git "$repository" rev-parse --verify "HEAD^{commit}"
  )" || {
    printf 'HEAD became unavailable while the release was being built.\n' >&2
    return 1
  }
  [[ "$current_commit" == "$expected_commit" ]] || {
    printf 'HEAD changed while the release was being built.\n' >&2
    return 1
  }
  current_tag_commit="$(
    run_source_git "$repository" \
      rev-parse --verify "refs/tags/$expected_tag^{commit}"
  )" || {
    printf 'Release tag %s became unavailable while building.\n' \
      "$expected_tag" >&2
    return 1
  }
  [[ "$current_tag_commit" == "$expected_commit" ]] || {
    printf 'Release tag %s changed while the release was being built.\n' \
      "$expected_tag" >&2
    return 1
  }
  current_version="$(
    run_source_git "$repository" show "$expected_commit:Cargo.toml" |
      sed -n 's/^version = "\([^"]*\)"/\1/p'
  )" || {
    printf 'Unable to re-read Cargo version from release source.\n' >&2
    return 1
  }
  [[ "$current_version" == "$expected_version" ]] || {
    printf 'Cargo version changed while the release was being built.\n' >&2
    return 1
  }
  worktree_state="$(
    run_source_git "$repository" status --porcelain --untracked-files=all
  )" || {
    printf 'Unable to re-check the release worktree state.\n' >&2
    return 1
  }
  [[ -z "$worktree_state" ]] || {
    printf 'Release worktree changed while the release was being built.\n' >&2
    return 1
  }
}

cleanup_private_nul_stream() {
  local status=$?
  local cleanup_failed=false
  local cleanup_path="${private_nul_stream_path-}"
  local cleanup_directory="${private_nul_stream_directory-}"

  trap - EXIT HUP INT TERM
  set +e
  if [[ -n "${private_nul_write_fd-}" ]]; then
    if ! exec {private_nul_write_fd}>&-; then
      cleanup_failed=true
    fi
    private_nul_write_fd=""
  fi
  if [[ -n "${private_nul_read_fd-}" ]]; then
    if ! exec {private_nul_read_fd}<&-; then
      cleanup_failed=true
    fi
    private_nul_read_fd=""
  fi
  if [[ -n "$cleanup_path" ]]; then
    if [[ "${cleanup_path%/*}" != "$cleanup_directory" ||
      "${cleanup_path##*/}" != dufs-release-verifier.* ]]
    then
      printf 'Refusing to remove unexpected verifier stream: %s\n' \
        "$cleanup_path" >&2
      cleanup_failed=true
    elif [[ -e "$cleanup_path" || -L "$cleanup_path" ]] &&
      ! rm -f -- "$cleanup_path"
    then
      cleanup_failed=true
    fi
  fi
  if [[ "$cleanup_failed" == true && "$status" -eq 0 ]]; then
    status=1
  fi
  exit "$status"
}

with_private_nul_stream() (
  local consumer="$1"
  local private_nul_stream_directory
  local private_nul_stream_path=""
  local private_nul_read_fd=""
  local private_nul_write_fd=""
  local producer_status
  shift

  trap cleanup_private_nul_stream EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM

  private_nul_stream_directory="$({
    cd -P -- "${TMPDIR:-/tmp}" && pwd -P
  })" || {
    printf 'Unable to resolve the verifier temporary directory.\n' >&2
    return 1
  }
  private_nul_stream_path="$(
    mktemp \
      --tmpdir="$private_nul_stream_directory" \
      dufs-release-verifier.XXXXXXXXXX
  )" || {
    printf 'Unable to create a private verifier stream.\n' >&2
    return 1
  }
  chmod 0600 -- "$private_nul_stream_path" || return $?
  exec {private_nul_write_fd}> "$private_nul_stream_path" || return $?
  exec {private_nul_read_fd}< "$private_nul_stream_path" || return $?
  rm -f -- "$private_nul_stream_path" || return $?
  private_nul_stream_path=""

  if "$@" >&"$private_nul_write_fd"; then
    :
  else
    producer_status=$?
    printf 'Release verifier producer failed with status %s.\n' \
      "$producer_status" >&2
    return "$producer_status"
  fi
  exec {private_nul_write_fd}>&-
  private_nul_write_fd=""
  "$consumer" "$private_nul_read_fd"
)

validate_release_tree_entry_stream() {
  local stream_fd="$1"
  local entry
  local metadata
  local mode
  local object_type
  local object_id
  local path

  while IFS= read -r -d '' -u "$stream_fd" entry; do
    metadata="${entry%%$'\t'*}"
    path="${entry#*$'\t'}"
    read -r mode object_type object_id <<< "$metadata"
    case "$mode:$object_type" in
      100644:blob|100755:blob) ;;
      120000:blob)
        printf 'Refusing symbolic link in release tree: %q\n' "$path" >&2
        return 1
        ;;
      160000:commit)
        printf 'Refusing submodule in release tree: %q\n' "$path" >&2
        return 1
        ;;
      *)
        printf \
          'Refusing unsupported Git entry mode/type %s/%s at %q.\n' \
          "$mode" \
          "$object_type" \
          "$path" >&2
        return 1
        ;;
    esac
    [[ -n "$object_id" ]] || {
      printf 'Release tree entry has no object ID: %q\n' "$path" >&2
      return 1
    }
  done
  [[ -z "$entry" ]] || {
    printf 'Release tree listing ended with a truncated entry: %q\n' \
      "$entry" >&2
    return 1
  }
}

validate_source_tree_entries() {
  local repository="$1"
  local commit="$2"

  with_private_nul_stream \
    validate_release_tree_entry_stream \
    run_source_git "$repository" ls-tree -rz --full-tree "$commit"
}

run_snapshot_git() {
  run_git_isolated --git-dir="$snapshot_git_directory" "$@"
}

validate_snapshot_git_metadata() {
  local metadata_path
  local current_checksum
  local replace_refs
  local resolved_commit
  local resolved_tree

  for metadata_path in \
    "$snapshot_git_directory/info/grafts" \
    "$snapshot_git_directory/info/attributes"
  do
    if [[ -e "$metadata_path" || -L "$metadata_path" ]]; then
      printf 'Isolated Git snapshot gained unsafe metadata: %s\n' \
        "$metadata_path" >&2
      return 1
    fi
  done
  replace_refs="$(
    run_snapshot_git for-each-ref --format='%(refname)' refs/replace/
  )"
  [[ -z "$replace_refs" ]] || {
    printf 'Isolated Git snapshot gained refs/replace entries.\n' >&2
    return 1
  }

  current_checksum="$(
    sha256sum < "$snapshot_git_directory/config"
  )"
  current_checksum="${current_checksum%% *}"
  [[ "$current_checksum" == "$snapshot_config_checksum" ]] || {
    printf 'Isolated Git snapshot configuration changed.\n' >&2
    return 1
  }
  current_checksum="$(
    sha256sum < "$snapshot_git_directory/objects/info/alternates"
  )"
  current_checksum="${current_checksum%% *}"
  [[ "$current_checksum" == "$snapshot_alternates_checksum" ]] || {
    printf 'Isolated Git snapshot object source changed.\n' >&2
    return 1
  }

  resolved_commit="$(
    run_snapshot_git rev-parse --verify "$source_sha^{commit}"
  )"
  resolved_tree="$(
    run_snapshot_git rev-parse --verify "$source_sha^{tree}"
  )"
  [[ "$resolved_commit" == "$source_sha" && "$resolved_tree" == "$source_tree" ]] || {
    printf 'Isolated Git snapshot no longer resolves the source tree.\n' >&2
    return 1
  }
}

validate_snapshot_tree_entries() {
  local commit="$1"

  with_private_nul_stream \
    validate_release_tree_entry_stream \
    run_snapshot_git ls-tree -rz --full-tree "$commit"
}

produce_extracted_source_tree_scan() {
  local extraction_directory="$1"

  # A valid prefix makes a partial-output-then-error producer distinguishable
  # from the safe, empty result of the unsafe-entry search below.
  printf 'dufs-extracted-tree-scan-v1\0' || return $?
  find \
    -P \
    "$extraction_directory" \
    -xdev \
    -mindepth 1 \
    \( -type l -o \( ! -type f ! -type d \) \) \
    -print0 \
    -quit
}

validate_extracted_source_tree_stream() {
  local stream_fd="$1"
  local marker=""
  local unsafe_path=""

  IFS= read -r -d '' -u "$stream_fd" marker || {
    printf 'Extracted source scan returned a truncated marker.\n' >&2
    return 1
  }
  [[ "$marker" == "dufs-extracted-tree-scan-v1" ]] || {
    printf 'Extracted source scan returned an invalid marker.\n' >&2
    return 1
  }
  if IFS= read -r -d '' -u "$stream_fd" unsafe_path; then
    printf 'Extracted source contains a link or special file: %q\n' \
      "$unsafe_path" >&2
    return 1
  fi
  [[ -z "$unsafe_path" ]] || {
    printf 'Extracted source scan returned a truncated path: %q\n' \
      "$unsafe_path" >&2
    return 1
  }
}

validate_extracted_source_tree() {
  local extraction_directory="$1"

  [[ -d "$extraction_directory" && ! -L "$extraction_directory" ]] || {
    printf 'Extracted source root is not a physical directory.\n' >&2
    return 1
  }
  with_private_nul_stream \
    validate_extracted_source_tree_stream \
    produce_extracted_source_tree_scan "$extraction_directory"
}

install_release_support_tree() {
  local source_root="$1"
  local package_root="$2"
  local entry
  local -a entries=(
    assets
    build.rs
    Cargo.lock
    Cargo.toml
    deploy
    docs
    LICENSE-APACHE
    package-lock.json
    package.json
    playwright.config.js
    README.md
    rust-toolchain.toml
    scripts
    SECURITY.md
    src
    tests
  )

  [[ -d "$source_root" && ! -L "$source_root" ]] || {
    printf 'Release support source is not a physical directory.\n' >&2
    return 1
  }
  [[ -d "$package_root" && ! -L "$package_root" ]] || {
    printf 'Release package root is not a physical directory.\n' >&2
    return 1
  }
  for entry in "${entries[@]}"; do
    [[ -e "$source_root/$entry" && ! -L "$source_root/$entry" ]] || {
      printf 'Release support source is missing: %s\n' "$entry" >&2
      return 1
    }
  done

  (
    cd "$source_root"
    tar \
      --create \
      --hard-dereference \
      --file=- \
      -- \
      "${entries[@]}"
  ) | (
    cd "$package_root"
    tar \
      --extract \
      --file=- \
      --no-same-owner \
      --same-permissions
  )
  find -P "$package_root" -xdev -type d \
    -exec chmod 0755 -- {} +
  find -P "$package_root" -xdev -type f -perm /0111 \
    -exec chmod 0755 -- {} +
  find -P "$package_root" -xdev -type f ! -perm /0111 \
    -exec chmod 0644 -- {} +
  validate_extracted_source_tree "$package_root"

}

verify_release_documentation_layout() {
  local package_root="$1"
  local node_command="$2"
  local required_path

  for required_path in \
    docs/README.md \
    docs/beginner-guide/README.md \
    docs/history/code-review-report.md
  do
    [[ -f "$package_root/$required_path" && \
      ! -L "$package_root/$required_path" ]] || {
      printf 'Release documentation layout is missing: %s\n' \
        "$required_path" >&2
      return 1
    }
  done
  [[ "$(stat -Lc '%a' -- "$package_root/docs")" == "755" && \
    "$(stat -Lc '%a' -- "$package_root/docs/README.md")" == "644" && \
    "$(stat -Lc '%a' -- "$package_root/scripts/package-release.sh")" == "755" ]] || {
    printf 'Release support tree has non-canonical public modes.\n' >&2
    return 1
  }

  run_node_entrypoint "$node_command" \
    "$package_root/scripts/check-docs.mjs" \
    --artifact-root "$package_root"
}

write_release_package_checksums() {
  local package_root="$1"

  (
    cd "$package_root"
    find \
      -P \
      . \
      -xdev \
      -type f \
      ! -path './SHA256SUMS' \
      -print0 |
      LC_ALL=C sort -z |
      while IFS= read -r -d '' package_file; do
        sha256sum -- "$package_file"
      done > SHA256SUMS
    chmod 0644 SHA256SUMS
    sha256sum --quiet --check SHA256SUMS
  )
}

verify_release_package_checksum_stream() {
  local stream_fd="$1"
  local checksum_line
  local package_file
  local expected_file_count=0
  local manifest_record_count=0

  while IFS= read -r -d '' -u "$stream_fd" package_file; do
    checksum_line="$(sha256sum -- "$package_file")"
    grep -Fqx -- "$checksum_line" SHA256SUMS || {
      printf 'Release checksum manifest omitted: %q\n' \
        "$package_file" >&2
      return 1
    }
    ((expected_file_count += 1))
  done
  [[ -z "$package_file" ]] || {
    printf 'Release checksum traversal ended with a truncated path: %q\n' \
      "$package_file" >&2
    return 1
  }
  while IFS= read -r checksum_line; do
    [[ -n "$checksum_line" ]] || {
      printf 'Release checksum manifest contains an empty record.\n' >&2
      return 1
    }
    ((manifest_record_count += 1))
  done < SHA256SUMS
  [[ "$manifest_record_count" -eq "$expected_file_count" ]] || {
    printf \
      'Release checksum manifest has %s records for %s package files.\n' \
      "$manifest_record_count" \
      "$expected_file_count" >&2
    return 1
  }
}

verify_release_package_checksum_coverage() {
  local package_root="$1"

  (
    cd "$package_root"
    sha256sum --quiet --check SHA256SUMS
    with_private_nul_stream \
      verify_release_package_checksum_stream \
      find \
        -P \
        . \
        -xdev \
        -type f \
        ! -path './SHA256SUMS' \
        -print0
  )
}

write_reproducible_release_archive() {
  local package_parent="$1"
  local package_name="$2"
  local release_epoch="$3"
  local archive_path="$4"

  (
    cd "$package_parent"
    tar \
      --sort=name \
      --mtime="@$release_epoch" \
      --owner=0 \
      --group=0 \
      --numeric-owner \
      -cf - \
      "$package_name" |
      gzip -n > "$archive_path"
  )
  chmod 0644 "$archive_path"
}

run_snapshot_worktree_git() {
  local worktree="$1"
  local index_file="$2"
  shift 2

  env -i \
    HOME=/ \
    XDG_CONFIG_HOME=/ \
    LANG=C \
    LC_ALL=C \
    PATH=/usr/bin:/bin \
    GIT_ATTR_NOSYSTEM=1 \
    GIT_CONFIG_GLOBAL=/dev/null \
    GIT_CONFIG_NOSYSTEM=1 \
    GIT_CONFIG_SYSTEM=/dev/null \
    GIT_DIR="$snapshot_git_directory" \
    GIT_INDEX_FILE="$index_file" \
    GIT_NO_REPLACE_OBJECTS=1 \
    GIT_WORK_TREE="$worktree" \
    "$git_command" \
    -c core.attributesFile=/dev/null \
    -c core.autocrlf=false \
    -c core.filemode=true \
    -c core.fsmonitor=false \
    -c core.symlinks=true \
    "$@"
}

initialize_source_snapshot() {
  local repository="$1"
  local destination="$2"
  local empty_template="$3"
  local commit="$4"
  local object_format
  local source_object_directory
  local snapshot_commit

  validate_source_git_metadata "$repository"
  source_object_directory="$(
    run_source_git "$repository" \
      rev-parse --path-format=absolute --git-path objects
  )"
  source_object_directory="$(realpath -e -- "$source_object_directory")"
  case "$source_object_directory" in
    *$'\n'*|*$'\r'*)
      printf 'Git object directory contains an unsupported character.\n' >&2
      return 1
      ;;
  esac
  object_format="$(
    run_source_git "$repository" rev-parse --show-object-format
  )"

  install -d -m 0700 "$empty_template"
  run_git_isolated init \
    --bare \
    --quiet \
    --template="$empty_template" \
    --object-format="$object_format" \
    "$destination"
  snapshot_git_directory="$destination"
  printf '%s\n' "$source_object_directory" \
    > "$snapshot_git_directory/objects/info/alternates"
  chmod 0600 "$snapshot_git_directory/objects/info/alternates"

  snapshot_commit="$(
    run_snapshot_git rev-parse --verify "$commit^{commit}"
  )"
  [[ "$snapshot_commit" == "$commit" ]] || {
    printf 'Isolated Git snapshot resolved an unexpected commit.\n' >&2
    return 1
  }
  run_snapshot_git update-ref refs/heads/release "$commit"
  source_tree="$(
    run_snapshot_git rev-parse --verify "$commit^{tree}"
  )"
  snapshot_config_checksum="$(
    sha256sum < "$snapshot_git_directory/config"
  )"
  snapshot_config_checksum="${snapshot_config_checksum%% *}"
  snapshot_alternates_checksum="$(
    sha256sum < "$snapshot_git_directory/objects/info/alternates"
  )"
  snapshot_alternates_checksum="${snapshot_alternates_checksum%% *}"
  validate_snapshot_git_metadata
}

create_and_verify_source_archive() {
  local archive_path="$1"
  local extraction_directory="$2"
  local verification_index="$3"
  local untracked_list="$4"
  local archive_commit

  validate_snapshot_git_metadata
  validate_snapshot_tree_entries "$source_sha"
  run_snapshot_git archive --format=tar "$source_sha" > "$archive_path"
  chmod 0600 "$archive_path"
  archive_commit="$(
    run_snapshot_git get-tar-commit-id < "$archive_path"
  )"
  [[ "$archive_commit" == "$source_sha" ]] || {
    printf 'Git archive does not identify source commit %s.\n' \
      "$source_sha" >&2
    return 1
  }

  tar -xf "$archive_path" -C "$extraction_directory"
  validate_extracted_source_tree "$extraction_directory"
  rm -f -- "$verification_index" "$verification_index.lock" "$untracked_list"
  run_snapshot_worktree_git \
    "$extraction_directory" \
    "$verification_index" \
    read-tree "$source_tree"
  # read-tree intentionally leaves the temporary index without worktree stat
  # data. Refresh it before diff-files so a matching archive is compared by
  # content and mode instead of being reported dirty solely for zeroed stats.
  if ! run_snapshot_worktree_git \
    "$extraction_directory" \
    "$verification_index" \
    update-index -q --refresh
  then
    :
  fi
  if ! run_snapshot_worktree_git \
    "$extraction_directory" \
    "$verification_index" \
    diff-files \
      --quiet \
      --no-ext-diff \
      --ignore-submodules=none \
      --
  then
    printf 'Extracted Git archive differs from source tree %s.\n' \
      "$source_tree" >&2
    return 1
  fi
  run_snapshot_worktree_git \
    "$extraction_directory" \
    "$verification_index" \
    ls-files \
      --others \
      --directory \
      --no-empty-directory \
      -- > "$untracked_list"
  [[ ! -s "$untracked_list" ]] || {
    printf 'Extracted Git archive contains paths outside source tree %s.\n' \
      "$source_tree" >&2
    return 1
  }
  rm -f -- "$verification_index" "$verification_index.lock" "$untracked_list"
}

verify_quality_source_after_gate() {
  local source_directory="$1"
  local verification_index="$2"
  local untracked_list="$3"

  validate_snapshot_git_metadata
  rm -f -- "$verification_index" "$verification_index.lock" "$untracked_list"
  run_snapshot_worktree_git \
    "$source_directory" \
    "$verification_index" \
    read-tree "$source_tree"
  if ! run_snapshot_worktree_git \
    "$source_directory" \
    "$verification_index" \
    update-index -q --refresh
  then
    :
  fi
  if ! run_snapshot_worktree_git \
    "$source_directory" \
    "$verification_index" \
    diff-files \
      --quiet \
      --no-ext-diff \
      --ignore-submodules=none \
      --
  then
    printf 'Quality checks changed a tracked source file from tree %s.\n' \
      "$source_tree" >&2
    return 1
  fi
  run_snapshot_worktree_git \
    "$source_directory" \
    "$verification_index" \
    ls-files \
      --others \
      --exclude-standard \
      -- > "$untracked_list"
  [[ ! -s "$untracked_list" ]] || {
    printf 'Quality checks created an unexpected source path:\n' >&2
    sed -n '1,40p' "$untracked_list" >&2
    return 1
  }
  rm -f -- "$verification_index" "$verification_index.lock" "$untracked_list"
}

expected_rust_library_notice_sha256() {
  local toolchain="$1"

  case "$toolchain" in
    1.97.1)
      printf '%s\n' \
        '0a65bb747c49c7bb816cbc7188319bd6e4e8d08091c1190b8a3c0971c47968ed'
      ;;
    *)
      printf \
        'No reviewed Rust standard-library notice digest for toolchain %s.\n' \
        "$toolchain" >&2
      return 1
      ;;
  esac
}

validate_contained_notice_file() {
  local trusted_root="$1"
  local candidate="$2"
  local expected_checksum="$3"
  local physical_root
  local physical_candidate
  local actual_checksum
  local raw_mode
  local numeric_raw_mode

  physical_root="$(realpath -e -- "$trusted_root")" || {
    printf 'Notice root is unavailable: %s\n' "$trusted_root" >&2
    return 1
  }
  [[ -d "$physical_root" && ! -L "$physical_root" ]] || {
    printf 'Notice root is not a physical directory: %s\n' "$trusted_root" >&2
    return 1
  }
  [[ -e "$candidate" && ! -L "$candidate" ]] || {
    printf 'Notice is unavailable or is a symbolic link: %s\n' \
      "$candidate" >&2
    return 1
  }
  raw_mode="$(stat -Lc '%f' -- "$candidate")"
  numeric_raw_mode=$((16#$raw_mode))
  (( (numeric_raw_mode & 0170000) == 0100000 )) || {
    printf 'Notice is not a regular file: %s\n' "$candidate" >&2
    return 1
  }
  physical_candidate="$(realpath -e -- "$candidate")" || {
    printf 'Notice cannot be resolved: %s\n' "$candidate" >&2
    return 1
  }
  case "$physical_candidate" in
    "$physical_root"/*) ;;
    *)
      printf 'Notice escapes its trusted root: %s\n' "$candidate" >&2
      return 1
      ;;
  esac
  actual_checksum="$(sha256sum < "$physical_candidate")"
  actual_checksum="${actual_checksum%% *}"
  [[ "$actual_checksum" == "$expected_checksum" ]] || {
    printf \
      'Notice checksum mismatch for %s: expected %s, found %s.\n' \
      "$physical_candidate" \
      "$expected_checksum" \
      "$actual_checksum" >&2
    return 1
  }
  printf '%s\n' "$physical_candidate"
}

locate_rust_library_notice() {
  local toolchain="$1"
  local rustc_path="$2"
  local expected_checksum="$3"
  local sysroot
  local physical_sysroot
  local notice

  sysroot="$(
    env RUSTUP_TOOLCHAIN="$toolchain" \
      "$rustc_path" --print sysroot
  )" || {
    printf 'Unable to locate the Rust %s sysroot.\n' "$toolchain" >&2
    return 1
  }
  [[ -n "$sysroot" && "$sysroot" != *$'\n'* ]] || {
    printf 'Rust returned an invalid sysroot for toolchain %s.\n' \
      "$toolchain" >&2
    return 1
  }
  physical_sysroot="$(realpath -e -- "$sysroot")" || {
    printf 'Rust sysroot is unavailable: %s\n' "$sysroot" >&2
    return 1
  }
  notice="$physical_sysroot/share/doc/rust/COPYRIGHT-library.html"
  validate_contained_notice_file \
    "$physical_sysroot" \
    "$notice" \
    "$expected_checksum"
}

atomic_publish_directory() {
  local source_directory="$1"
  local destination_directory="$2"
  local destination_parent="${destination_directory%/*}"
  local source_device
  local source_identity
  local destination_device
  local destination_identity=""
  local move_status=0

  [[ -d "$source_directory" && ! -L "$source_directory" ]] || {
    printf 'Staged release directory is unavailable: %s\n' "$source_directory" >&2
    return 1
  }
  source_identity="$(stat -Lc '%d:%i' -- "$source_directory")" || {
    printf 'Unable to identify staged release directory: %s\n' \
      "$source_directory" >&2
    return 1
  }
  source_device="${source_identity%%:*}"
  destination_device="$(stat -Lc '%d' -- "$destination_parent")" || {
    printf 'Unable to identify release output directory: %s\n' \
      "$destination_parent" >&2
    return 1
  }
  [[ "$source_device" == "$destination_device" ]] || {
    printf 'Staged and final release directories are on different filesystems.\n' >&2
    return 1
  }

  # --no-copy makes a cross-filesystem fallback impossible. GNU mv's
  # --update=none deliberately exits successfully when it skips an existing
  # destination, so the source postcondition below turns that silent skip
  # into a hard collision failure without requiring coreutils 9.5's
  # --update=none-fail spelling.
  mv \
    --no-copy \
    --update=none \
    --no-target-directory \
    -- \
    "$source_directory" \
    "$destination_directory" || move_status=$?
  if [[ ! -e "$source_directory" && ! -L "$source_directory" && \
    -d "$destination_directory" && ! -L "$destination_directory" ]]
  then
    destination_identity="$(stat -Lc '%d:%i' -- "$destination_directory")" || \
      destination_identity=""
    if [[ "$destination_identity" == "$source_identity" ]]; then
      return 0
    fi
  fi
  printf 'Refusing to replace release directory (mv status %s): %s\n' \
    "$move_status" "$destination_directory" >&2
  return 1
}

publish_release_directory_durably() {
  local source_directory="$1"
  local destination_directory="$2"
  local output_directory="$3"
  local self_test_signal_after_rename="${4:-false}"
  local publish_status=0

  # A caught signal can make Bash run its trap as soon as an external command
  # returns. Ignore ordinary termination signals for the short namespace
  # commit so mv and sync inherit SIG_IGN too. This prevents TERM/INT/HUP from
  # exposing a renamed directory whose parent-directory sync was skipped.
  # SIGKILL, host power loss and an actual sync error remain explicit limits.
  trap '' HUP INT TERM
  atomic_publish_directory \
    "$source_directory" \
    "$destination_directory" || publish_status=$?
  if [[ "$publish_status" -eq 0 ]]; then
    if [[ "$self_test_signal_after_rename" == true ]]; then
      kill -TERM "$$"
    fi
    sync -- "$output_directory" || publish_status=$?
  fi
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM
  return "$publish_status"
}

sign_and_verify_checksum() {
  local signing_key_path="$1"
  local checksum_path="$2"
  local signature_path="$3"
  local public_key_path="$4"
  local key_description
  local key_bits
  local ec_curve

  openssl pkey \
    -in "$signing_key_path" \
    -pubout \
    -out "$public_key_path" || return $?
  key_description="$(
    LC_ALL=C openssl pkey \
      -pubin \
      -in "$public_key_path" \
      -text_pub \
      -noout
  )" || return $?

  case "$key_description" in
    ED25519*|ED448*)
      signature_mode="EdDSA raw message"
      openssl pkeyutl \
        -sign \
        -rawin \
        -inkey "$signing_key_path" \
        -in "$checksum_path" \
        -out "$signature_path" || return $?
      openssl pkeyutl \
        -verify \
        -rawin \
        -pubin \
        -inkey "$public_key_path" \
        -sigfile "$signature_path" \
        -in "$checksum_path" >/dev/null || return $?
      return
      ;;
    *)
      key_bits="$(
        sed -nE 's/^Public-Key: \(([0-9]+) bit\)$/\1/p' \
          <<< "$key_description"
      )" || return $?
      if [[ "$key_description" == *$'\nModulus:'* ]]; then
        [[ "$key_bits" =~ ^[0-9]+$ ]] || {
          printf 'Unable to determine RSA signing-key strength.\n' >&2
          return 1
        }
        (( key_bits >= 3072 )) || {
          printf 'RSA signing keys must be at least 3072 bits; found %s.\n' \
            "$key_bits" >&2
          return 1
        }
      else
        ec_curve="$(
          sed -n 's/^ASN1 OID: //p' <<< "$key_description"
        )" || return $?
        case "$ec_curve" in
          prime256v1|secp384r1|secp521r1) ;;
          "")
            printf 'Unsupported release signing-key algorithm.\n' >&2
            return 1
            ;;
          *)
            printf 'EC signing curve is not approved for releases: %s\n' \
              "$ec_curve" >&2
            return 1
            ;;
        esac
      fi
      ;;
  esac

  signature_mode="SHA-256 digest"
  openssl dgst \
    -sha256 \
    -sign "$signing_key_path" \
    -out "$signature_path" \
    "$checksum_path" || return $?
  openssl dgst \
    -sha256 \
    -verify "$public_key_path" \
    -signature "$signature_path" \
    "$checksum_path" >/dev/null || return $?
}

sign_checksum_with_validated_key() {
  local signing_key_argument_value="$1"
  local current_uid="$2"
  local checksum_path="$3"
  local signature_path="$4"
  local public_key_path="$5"
  local signature_mode_path="$6"

  # This subshell is intentionally entered only after every invoked build-time
  # and packaging program has returned. Only the short-lived validation and
  # OpenSSL children below inherit the key fd. Any process under the same uid,
  # including a background descendant left by build code, can still read an
  # operator-readable key by pathname; use a separate signing identity or host
  # when build inputs are not fully trusted.
  (
    local signing_key_candidate
    local signing_key
    local signing_key_fd
    local signing_key_handle
    local signing_key_identity
    local opened_key_identity
    local path_key_identity

    signing_key_candidate="$(
      resolve_invocation_path "$signing_key_argument_value"
    )" || return $?
    signing_key="$(realpath -e -- "$signing_key_candidate")" || {
      printf 'A readable PEM signing key is required: %s\n' \
        "$signing_key_candidate" >&2
      return 1
    }
    [[ -f "$signing_key" && -r "$signing_key" ]] || {
      printf 'A readable PEM signing key is required: %s\n' \
        "$signing_key_candidate" >&2
      return 1
    }
    signing_key_identity="$(stat -Lc '%d:%i' -- "$signing_key")" || return $?
    exec {signing_key_fd}<"$signing_key" || return $?
    signing_key_handle="/proc/self/fd/$signing_key_fd"
    validate_signing_key "$signing_key_handle" "$current_uid" || return $?
    opened_key_identity="$(
      stat -Lc '%d:%i' -- "$signing_key_handle"
    )" || return $?
    [[ "$opened_key_identity" == "$signing_key_identity" ]] || {
      printf 'Signing key changed while it was being opened.\n' >&2
      return 1
    }
    path_key_identity="$(stat -Lc '%d:%i' -- "$signing_key")" || return $?
    [[ "$path_key_identity" == "$signing_key_identity" ]] || {
      printf 'Signing key path changed while it was being opened.\n' >&2
      return 1
    }

    signature_mode=""
    sign_and_verify_checksum \
      "$signing_key_handle" \
      "$checksum_path" \
      "$signature_path" \
      "$public_key_path" || return $?
    printf '%s\n' "$signature_mode" > "$signature_mode_path" || return $?
    exec {signing_key_fd}>&- || return $?
  )
}

cleanup_path=""
cleanup_parent=""
cleanup_prefix=""
release_lock_fd=""

remove_private_cleanup_tree() {
  local path="$cleanup_path"
  local parent="${path%/*}"
  local base="${path##*/}"

  [[ -n "$path" ]] || return 0
  if [[ "$parent" != "$cleanup_parent" || "$base" != "$cleanup_prefix"* ]]; then
    printf 'Refusing to remove unexpected cleanup path: %s\n' "$path" >&2
    return 1
  fi
  if [[ -e "$path" || -L "$path" ]]; then
    rm -rf --one-file-system -- "$path"
  fi
}

cleanup_release() {
  local status=$?
  local cleanup_failed=false

  trap - EXIT HUP INT TERM
  set +e
  if ! remove_private_cleanup_tree; then
    cleanup_failed=true
  fi
  cleanup_path=""
  if [[ -n "$release_lock_fd" ]]; then
    exec {release_lock_fd}>&-
    release_lock_fd=""
  fi
  if [[ "$cleanup_failed" == true && "$status" -eq 0 ]]; then
    status=1
  fi
  exit "$status"
}

trap cleanup_release EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

run_publication_self_test() {
  local test_root
  local test_output
  local unsupported_output
  local test_alias
  local physical_output
  local staged_parent
  local staged_release
  local final_release
  local collision_source
  local collision_destination
  local protected_directory
  local symlink_source
  local symlink_destination
  local current_uid
  local test_lock_fd
  local checksum
  local signature
  local public_key
  local rsa_key
  local weak_rsa_key
  local ed25519_key
  local ed448_key
  local ec_key
  local unapproved_ec_key
  local dsa_parameters
  local dsa_key
  local unknown_key
  local linked_key
  local signature_mode_file
  local locked_output
  local locked_staged_release
  local locked_final_release
  local original_output_identity
  local original_output_metadata
  local moved_output
  local replacement_output
  local test_repository
  local test_git_template
  local original_commit
  local replacement_commit
  local repository_git_directory
  local snapshot_directory
  local snapshot_template
  local first_source_archive
  local second_source_archive
  local first_source_tree
  local second_source_tree
  local first_archive_checksum
  local second_archive_checksum
  local unsafe_commit
  local unsafe_parent
  local unsafe_path
  local previous_unsafe_path=""
  local unsafe_target
  local extracted_safety_test
  local notice_test_root
  local notice_test_file
  local notice_test_checksum
  local notice_test_outside
  local notice_test_link
  local build_environment_manifest
  local documentation_package
  local documentation_package_name
  local documentation_sentinel
  local documentation_archive_one
  local documentation_archive_two
  local documentation_archive_checksum_one
  local documentation_archive_checksum_two
  local documentation_unpack_root
  local advisory_database_test
  local advisory_database_test_revision
  local advisory_database_test_fetch_epoch
  local advisory_database_test_classification
  local advisory_database_test_status
  local advisory_database_test_state
  local advisory_database_test_index_checksum
  local advisory_database_test_config_checksum
  local advisory_database_filter_marker
  local advisory_database_test_mode
  local advisory_database_untracked_directory
  local freshness_current_epoch
  local freshness_expected_status
  local freshness_fetch_epoch
  local freshness_status
  local partial_git
  local partial_find_bin
  local partial_find
  local verifier_temporary_directory
  local verifier_leak

  current_uid="$(id -u)"
  test_root="$(mktemp -d -t dufs-release-self-test.XXXXXXXX)"
  chmod 0700 "$test_root"
  cleanup_path="$test_root"
  cleanup_parent="${test_root%/*}"
  cleanup_prefix="dufs-release-self-test."

  validate_cargo_audit_version_line \
    'cargo-audit-audit 0.22.2' \
    "$required_cargo_audit_version" || {
    printf 'release self-test rejected the pinned cargo-audit version\n' >&2
    return 1
  }
  if validate_cargo_audit_version_line \
    'cargo-audit-audit 0.22.1' \
    "$required_cargo_audit_version"
  then
    printf 'release self-test accepted an unpinned cargo-audit version\n' >&2
    return 1
  fi

  while read -r \
    freshness_fetch_epoch freshness_current_epoch freshness_expected_status
  do
    freshness_status=0
    validate_advisory_database_freshness \
      "$freshness_fetch_epoch" \
      "$freshness_current_epoch" \
      "$rustsec_advisory_database_maximum_age_seconds" \
      "$rustsec_advisory_database_maximum_future_skew_seconds" || \
      freshness_status=$?
    [[ "$freshness_status" == "$freshness_expected_status" ]] || {
      printf \
        'release self-test classified RustSec freshness case %s/%s as %s, expected %s\n' \
        "$freshness_fetch_epoch" \
        "$freshness_current_epoch" \
        "$freshness_status" \
        "$freshness_expected_status" >&2
      return 1
    }
  done <<EOF
1000000 1000000 0
$((1000000 - rustsec_advisory_database_maximum_age_seconds)) 1000000 0
$((999999 - rustsec_advisory_database_maximum_age_seconds)) 1000000 1
$((1000000 + rustsec_advisory_database_maximum_future_skew_seconds)) 1000000 0
$((1000001 + rustsec_advisory_database_maximum_future_skew_seconds)) 1000000 2
invalid 1000000 2
EOF

  advisory_database_test="$test_root/advisory-database"
  run_git_isolated init --quiet "$advisory_database_test"
  printf 'README.md filter=release-self-test-filter\n' > \
    "$advisory_database_test/.gitattributes"
  printf 'self-test\n' > "$advisory_database_test/README.md"
  run_git_isolated \
    -C "$advisory_database_test" \
    add .gitattributes README.md
  run_git_isolated \
    -C "$advisory_database_test" \
    -c user.name=release-self-test \
    -c user.email=release-self-test@example.invalid \
    commit --quiet -m initial
  run_git_isolated \
    -C "$advisory_database_test" \
    remote add origin "$rustsec_advisory_database_url"
  advisory_database_test_revision="$(
    advisory_database_revision "$advisory_database_test"
  )"
  printf '%s\t\t%s\n' \
    "$advisory_database_test_revision" \
    "$rustsec_advisory_database_url" > \
    "$advisory_database_test/.git/FETCH_HEAD"
  advisory_database_test_classification="$(
    classify_advisory_database_identity "$advisory_database_test"
  )"
  read -r \
    advisory_database_test_status \
    advisory_database_test_revision \
    advisory_database_test_fetch_epoch <<< \
    "$advisory_database_test_classification"
  [[ "$advisory_database_test_status" == "reusable" &&
    "$advisory_database_test_revision" =~ ^[0-9a-f]{40}$ &&
    "$advisory_database_test_fetch_epoch" =~ ^[0-9]{1,12}$ ]] || {
    printf 'release self-test produced an invalid RustSec database identity\n' >&2
    return 1
  }
  advisory_database_filter_marker="$test_root/rustsec-filter-ran"
  run_git_isolated \
    -C "$advisory_database_test" \
    config \
    filter.release-self-test-filter.clean \
    "tee -- $advisory_database_filter_marker"
  validate_advisory_database_state \
    "$advisory_database_test" >/dev/null || return $?
  [[ ! -e "$advisory_database_filter_marker" ]] || {
    printf 'release self-test executed a RustSec repository clean filter\n' >&2
    return 1
  }
  run_git_isolated \
    -C "$advisory_database_test" \
    config --unset-all filter.release-self-test-filter.clean
  advisory_database_test_state="$(
    validate_advisory_database_state "$advisory_database_test"
  )" || return $?
  read -r \
    advisory_database_test_revision \
    advisory_database_test_fetch_epoch \
    advisory_database_test_index_checksum \
    advisory_database_test_config_checksum <<< \
    "$advisory_database_test_state"

  printf 'changed after the RustSec seal\n' > "$advisory_database_test/README.md"
  if validate_advisory_database_state \
    "$advisory_database_test" \
    "$advisory_database_test_revision" \
    "$advisory_database_test_fetch_epoch" \
    "$advisory_database_test_index_checksum" \
    "$advisory_database_test_config_checksum" >/dev/null 2>&1
  then
    printf 'release self-test accepted a modified tracked RustSec advisory\n' >&2
    return 1
  fi
  printf 'self-test\n' > "$advisory_database_test/README.md"

  advisory_database_test_mode="$(
    stat -Lc '%a' -- "$advisory_database_test/README.md"
  )"
  chmod u+x -- "$advisory_database_test/README.md"
  if validate_advisory_database_state \
    "$advisory_database_test" \
    "$advisory_database_test_revision" \
    "$advisory_database_test_fetch_epoch" \
    "$advisory_database_test_index_checksum" \
    "$advisory_database_test_config_checksum" >/dev/null 2>&1
  then
    printf 'release self-test accepted a RustSec tracked-mode change\n' >&2
    return 1
  fi
  chmod "$advisory_database_test_mode" -- "$advisory_database_test/README.md"

  advisory_database_untracked_directory="$advisory_database_test/crates/release-self-test"
  install -d -m 0700 "$advisory_database_untracked_directory"
  printf 'untracked advisory\n' > \
    "$advisory_database_untracked_directory/RUSTSEC-9999-9999.toml"
  if validate_advisory_database_state \
    "$advisory_database_test" \
    "$advisory_database_test_revision" \
    "$advisory_database_test_fetch_epoch" \
    "$advisory_database_test_index_checksum" \
    "$advisory_database_test_config_checksum" >/dev/null 2>&1
  then
    printf 'release self-test accepted an untracked RustSec advisory\n' >&2
    return 1
  fi
  rm -f -- "$advisory_database_untracked_directory/RUSTSEC-9999-9999.toml"
  rmdir -- "$advisory_database_untracked_directory" \
    "$advisory_database_test/crates"

  printf 'staged index change\n' > "$advisory_database_test/README.md"
  run_git_isolated -C "$advisory_database_test" add README.md
  printf 'self-test\n' > "$advisory_database_test/README.md"
  if validate_advisory_database_state \
    "$advisory_database_test" \
    "$advisory_database_test_revision" \
    "$advisory_database_test_fetch_epoch" \
    "$advisory_database_test_index_checksum" \
    "$advisory_database_test_config_checksum" >/dev/null 2>&1
  then
    printf 'release self-test accepted a modified RustSec Git index\n' >&2
    return 1
  fi
  run_git_isolated \
    -C "$advisory_database_test" \
    reset --quiet HEAD -- README.md
  advisory_database_test_state="$(
    validate_advisory_database_state \
      "$advisory_database_test" \
      "$advisory_database_test_revision" \
      "$advisory_database_test_fetch_epoch"
  )" || return $?
  read -r \
    advisory_database_test_revision \
    advisory_database_test_fetch_epoch \
    advisory_database_test_index_checksum \
    advisory_database_test_config_checksum <<< \
    "$advisory_database_test_state"

  printf 'README.md export-ignore\n' > \
    "$advisory_database_test/.git/info/attributes"
  if validate_advisory_database_state \
    "$advisory_database_test" \
    "$advisory_database_test_revision" \
    "$advisory_database_test_fetch_epoch" \
    "$advisory_database_test_index_checksum" \
    "$advisory_database_test_config_checksum" >/dev/null 2>&1
  then
    printf 'release self-test accepted unsafe RustSec Git metadata\n' >&2
    return 1
  fi
  rm -f -- "$advisory_database_test/.git/info/attributes"
  validate_advisory_database_state \
    "$advisory_database_test" \
    "$advisory_database_test_revision" \
    "$advisory_database_test_fetch_epoch" \
    "$advisory_database_test_index_checksum" \
    "$advisory_database_test_config_checksum" >/dev/null

  # Exercise the formal-release sequence as one chain: a successful pre-gate
  # seal must still reject an otherwise-successful gate that leaves any new
  # database state behind.
  advisory_database_test_state="$(
    validate_advisory_database_state "$advisory_database_test"
  )" || return $?
  read -r \
    advisory_database_test_revision \
    advisory_database_test_fetch_epoch \
    advisory_database_test_index_checksum \
    advisory_database_test_config_checksum <<< \
    "$advisory_database_test_state"
  (
    printf 'simulated quality-gate output\n' > \
      "$advisory_database_test/quality-gate-output"
  )
  if validate_advisory_database_state \
    "$advisory_database_test" \
    "$advisory_database_test_revision" \
    "$advisory_database_test_fetch_epoch" \
    "$advisory_database_test_index_checksum" \
    "$advisory_database_test_config_checksum" >/dev/null 2>&1
  then
    printf '%s\n' \
      'release self-test accepted RustSec state changed by the simulated quality gate' >&2
    return 1
  fi
  rm -f -- "$advisory_database_test/quality-gate-output"
  validate_advisory_database_state \
    "$advisory_database_test" \
    "$advisory_database_test_revision" \
    "$advisory_database_test_fetch_epoch" \
    "$advisory_database_test_index_checksum" \
    "$advisory_database_test_config_checksum" >/dev/null

  run_git_isolated \
    -C "$advisory_database_test" \
    remote set-url origin https://example.invalid/advisory-db.git
  [[ "$(
    classify_advisory_database_identity \
      "$advisory_database_test" 2>/dev/null
  )" == "unavailable" ]] || {
    printf 'release self-test classified an untrusted RustSec origin as reusable\n' >&2
    return 1
  }
  run_git_isolated \
    -C "$advisory_database_test" \
    remote set-url origin "$rustsec_advisory_database_url"
  rm -f -- "$advisory_database_test/.git/FETCH_HEAD"
  [[ "$(
    classify_advisory_database_identity \
      "$advisory_database_test" 2>/dev/null
  )" == "unavailable" ]] || {
    printf 'release self-test classified a missing RustSec FETCH_HEAD as reusable\n' >&2
    return 1
  }
  printf '%s\t\t%s\n' \
    0000000000000000000000000000000000000000 \
    "$rustsec_advisory_database_url" > \
    "$advisory_database_test/.git/FETCH_HEAD"
  [[ "$(
    classify_advisory_database_identity \
      "$advisory_database_test" 2>/dev/null
  )" == "unavailable" ]] || {
    printf 'release self-test classified a mismatched RustSec FETCH_HEAD as reusable\n' >&2
    return 1
  }

  build_environment_manifest="$test_root/BUILD-ENVIRONMENT.txt"
  write_build_environment_manifest \
    "$build_environment_manifest" \
    0123456789abcdef0123456789abcdef01234567 \
    0.0.0-test \
    1234567890 \
    test-target \
    'rustc 1.97.1 (test)' \
    'cargo 1.97.1 (test)' \
    'cargo-cyclonedx-cyclonedx 0.5.9' \
    'cargo-audit-audit 0.22.2' \
    0123456789abcdef0123456789abcdef01234567 \
    1234567890 \
    "$node_command" \
    "$npm_command"
  grep -Fxq \
    'format=dufs-build-environment-v2' \
    "$build_environment_manifest"
  grep -Fxq \
    'source_sha=0123456789abcdef0123456789abcdef01234567' \
    "$build_environment_manifest"
  grep -Fxq 'source_version=0.0.0-test' "$build_environment_manifest"
  grep -Fxq 'source_date_epoch=1234567890' "$build_environment_manifest"
  grep -Fxq 'target=test-target' "$build_environment_manifest"
  local manifest_key
  for manifest_key in \
    bash \
    rustc \
    cargo \
    cargo_cyclonedx \
    cargo_audit \
    rustsec_advisory_db_revision \
    rustsec_advisory_db_fetch_epoch \
    node \
    npm \
    git \
    openssl \
    tar \
    gzip \
    mv \
    sha256sum
  do
    grep -Eq "^${manifest_key}=.+$" "$build_environment_manifest" || {
      printf 'release self-test build manifest omitted %s\n' \
        "$manifest_key" >&2
      return 1
    }
  done
  [[ "$(stat -Lc '%a' -- "$build_environment_manifest")" == "644" ]] || {
    printf 'release self-test produced an unsafe build manifest mode\n' >&2
    return 1
  }

  documentation_package_name="documentation-package"
  documentation_package="$test_root/$documentation_package_name"
  install -d -m 0700 "$documentation_package"
  install_release_support_tree "$project_dir" "$documentation_package"
  verify_release_documentation_layout \
    "$documentation_package" \
    "$node_command"
  documentation_sentinel="$documentation_package/docs/beginner-guide/.release-self-test-sentinel"
  printf 'recursive release checksum sentinel\n' > "$documentation_sentinel"
  chmod 0644 "$documentation_sentinel"
  write_release_package_checksums "$documentation_package"
  verify_release_package_checksum_coverage "$documentation_package"
  printf 'tampered\n' >> "$documentation_sentinel"
  if (
    cd "$documentation_package"
    sha256sum --quiet --check SHA256SUMS >/dev/null 2>&1
  ); then
    printf 'release self-test checksum did not detect tampering\n' >&2
    return 1
  fi
  printf 'recursive release checksum sentinel\n' > "$documentation_sentinel"
  verify_release_package_checksum_coverage "$documentation_package"

  documentation_archive_one="$test_root/documentation-package-one.tar.gz"
  documentation_archive_two="$test_root/documentation-package-two.tar.gz"
  write_reproducible_release_archive \
    "$test_root" \
    "$documentation_package_name" \
    1234567890 \
    "$documentation_archive_one"
  write_reproducible_release_archive \
    "$test_root" \
    "$documentation_package_name" \
    1234567890 \
    "$documentation_archive_two"
  documentation_archive_checksum_one="$(sha256sum < "$documentation_archive_one")"
  documentation_archive_checksum_two="$(sha256sum < "$documentation_archive_two")"
  [[ "$documentation_archive_checksum_one" == \
    "$documentation_archive_checksum_two" ]] || {
    printf 'release self-test documentation archives were not reproducible\n' >&2
    return 1
  }
  documentation_unpack_root="$test_root/documentation-unpacked"
  install -d -m 0700 "$documentation_unpack_root"
  gzip --decompress --stdout -- "$documentation_archive_one" |
    tar \
      --extract \
      --file=- \
      --directory="$documentation_unpack_root" \
      --no-same-owner \
      --same-permissions
  verify_release_documentation_layout \
    "$documentation_unpack_root/$documentation_package_name" \
    "$node_command"
  verify_release_package_checksum_coverage \
    "$documentation_unpack_root/$documentation_package_name"

  unsupported_output="$test_root/output\\unsupported"
  install -d -m 0700 "$unsupported_output"
  if validate_output_directory \
    "$unsupported_output" \
    "$current_uid" 2>/dev/null
  then
    printf 'release self-test accepted an output path containing a backslash\n' >&2
    return 1
  fi

  test_output="$test_root/output"
  test_alias="$test_root/output-alias"
  install -d -m 0700 "$test_output"
  ln -s -- "$test_output" "$test_alias"
  physical_output="$(
    cd -P -- "$test_alias"
    pwd -P
  )"
  [[ "$(realpath -e -- "$physical_output")" == \
    "$(realpath -e -- "$test_output")" ]] || {
    printf 'release self-test did not resolve the physical output directory\n' >&2
    return 1
  }
  validate_output_directory "$physical_output" "$current_uid"
  if validate_output_directory \
    "$physical_output" \
    "$((current_uid + 1))" 2>/dev/null
  then
    printf 'release self-test accepted an output owned by another uid\n' >&2
    return 1
  fi

  chmod 0770 "$physical_output"
  if validate_output_directory "$physical_output" "$current_uid" 2>/dev/null; then
    printf 'release self-test accepted a group-writable output directory\n' >&2
    return 1
  fi
  chmod 0700 "$physical_output"

  exec {test_lock_fd}<"$physical_output"
  flock --exclusive "$test_lock_fd"
  locked_output="/proc/$packager_pid/fd/$test_lock_fd"
  original_output_identity="$(stat -Lc '%d:%i' -- "$locked_output")"
  original_output_metadata="$(stat -Lc '%u:%a:%d:%i' -- "$locked_output")"
  if ! (
    exec {test_lock_fd}>&-
    [[ "$(stat -Lc '%d:%i' -- "$locked_output")" == \
      "$original_output_identity" ]]
  ); then
    printf '%s\n' \
      'release self-test lost its fixed-process output anchor after closing the inherited fd' >&2
    return 1
  fi
  validate_private_directory_binding \
    "$locked_output" \
    "$physical_output" \
    "$original_output_metadata" \
    'release self-test output'
  if flock --exclusive --nonblock "$physical_output" true 2>/dev/null; then
    printf 'release self-test did not serialize the output directory\n' >&2
    return 1
  fi

  # Rebind the public pathname before creating any stage content. Stage
  # creation and every later write must still resolve below the locked fd.
  moved_output="$test_root/original-output"
  replacement_output="$physical_output"
  mv -T -- "$physical_output" "$moved_output"
  install -d -m 0700 "$replacement_output"
  if validate_private_directory_binding \
    "$locked_output" \
    "$physical_output" \
    "$original_output_metadata" \
    'release self-test output' 2>/dev/null
  then
    printf 'release self-test did not detect a private path rebinding\n' >&2
    return 1
  fi

  staged_parent="$(
    mktemp -d --tmpdir="$locked_output" .dufs-release-stage.XXXXXXXX
  )"
  chmod 0700 "$staged_parent"
  [[ -d "$moved_output/${staged_parent##*/}" ]] || {
    printf 'release self-test did not create its stage through the locked fd\n' >&2
    return 1
  }
  [[ ! -e "$replacement_output/${staged_parent##*/}" ]] || {
    printf 'release self-test created a stage through the rebound path\n' >&2
    return 1
  }
  staged_release="$staged_parent/example.release"
  final_release="$physical_output/example.release"
  locked_staged_release="$staged_release"
  locked_final_release="$locked_output/example.release"
  install -d -m 0755 "$staged_release"
  printf 'archive\n' > "$staged_release/example.tar.gz"
  printf 'checksum\n' > "$staged_release/example.tar.gz.sha256"
  printf 'signature\n' > "$staged_release/example.tar.gz.sha256.sig"
  printf 'public key\n' > "$staged_release/example.tar.gz.sha256.pub.pem"
  sync -- "$staged_release"/*
  sync -- "$staged_release"

  # Publication must target the original inode through /proc/self/fd, and the
  # public identity check must report the earlier path substitution.
  # Send TERM at the exact former gap between rename and parent-directory
  # sync. The durable publication helper must finish and preserve the release.
  publish_release_directory_durably \
    "$locked_staged_release" \
    "$locked_final_release" \
    "$locked_output" \
    true
  if validate_public_output_binding \
    "$replacement_output" \
    "$locked_output" \
    "$original_output_identity" 2>/dev/null
  then
    printf 'release self-test did not detect output path rebinding\n' >&2
    return 1
  fi
  [[ -d "$moved_output/example.release" ]] || {
    printf 'release self-test did not publish through the locked directory fd\n' >&2
    return 1
  }
  [[ ! -e "$replacement_output/example.release" ]] || {
    printf 'release self-test published through the rebound string path\n' >&2
    return 1
  }
  rm -rf --one-file-system -- "$replacement_output"
  mv -T -- "$moved_output" "$physical_output"
  validate_public_output_binding \
    "$physical_output" \
    "$locked_output" \
    "$original_output_identity"
  validate_private_directory_binding \
    "$locked_output" \
    "$physical_output" \
    "$original_output_metadata" \
    'release self-test output'
  [[ -d "$final_release" && ! -e "$staged_release" ]] || {
    printf 'release self-test did not publish one complete directory\n' >&2
    return 1
  }

  collision_source="$staged_parent/collision-source.release"
  collision_destination="$physical_output/collision.release"
  install -d -m 0755 "$collision_source" "$collision_destination"
  printf 'source\n' > "$collision_source/value"
  # Keep the destination empty: an ordinary directory rename could replace
  # it, so this proves the no-replace option rather than relying on ENOTEMPTY.
  if atomic_publish_directory \
    "$collision_source" \
    "$collision_destination" 2>/dev/null
  then
    printf 'release self-test overwrote an existing directory\n' >&2
    return 1
  fi
  [[ "$(<"$collision_source/value")" == "source" ]] || return 1
  [[ -d "$collision_destination" && ! -L "$collision_destination" ]] || \
    return 1
  [[ ! -e "$collision_destination/value" && \
    ! -L "$collision_destination/value" ]] || return 1

  protected_directory="$test_root/protected"
  symlink_source="$staged_parent/symlink-source.release"
  symlink_destination="$physical_output/symlink.release"
  install -d -m 0700 "$protected_directory"
  install -d -m 0755 "$symlink_source"
  printf 'protected\n' > "$protected_directory/value"
  ln -s -- "$protected_directory" "$symlink_destination"
  if atomic_publish_directory \
    "$symlink_source" \
    "$symlink_destination" 2>/dev/null
  then
    printf 'release self-test followed an existing destination symlink\n' >&2
    return 1
  fi
  [[ "$(<"$protected_directory/value")" == "protected" ]] || return 1
  [[ -d "$symlink_source" && -L "$symlink_destination" ]] || return 1

  checksum="$test_root/checksum"
  printf '0123456789abcdef  example.tar.gz\n' > "$checksum"
  rsa_key="$test_root/rsa.pem"
  signature="$test_root/rsa.sig"
  public_key="$test_root/rsa.pub.pem"
  openssl genpkey \
    -algorithm RSA \
    -pkeyopt rsa_keygen_bits:3072 \
    -out "$rsa_key" 2>/dev/null
  chmod 0600 "$rsa_key"
  validate_signing_key "$rsa_key" "$current_uid"
  if validate_signing_key "$rsa_key" "$((current_uid + 1))" 2>/dev/null; then
    printf 'release self-test accepted a key owned by another uid\n' >&2
    return 1
  fi
  chmod 0640 "$rsa_key"
  if validate_signing_key "$rsa_key" "$current_uid" 2>/dev/null; then
    printf 'release self-test accepted an exposed signing key\n' >&2
    return 1
  fi
  chmod 0600 "$rsa_key"
  linked_key="$test_root/rsa-linked.pem"
  ln -- "$rsa_key" "$linked_key"
  if validate_signing_key "$rsa_key" "$current_uid" 2>/dev/null; then
    printf 'release self-test accepted a multiply linked signing key\n' >&2
    return 1
  fi
  rm -f -- "$linked_key"
  signature_mode_file="$test_root/rsa.mode"
  sign_checksum_with_validated_key \
    "$rsa_key" \
    "$current_uid" \
    "$checksum" \
    "$signature" \
    "$public_key" \
    "$signature_mode_file"
  signature_mode="$(<"$signature_mode_file")"
  rm -f -- "$signature_mode_file"
  [[ "$signature_mode" == "SHA-256 digest" ]] || return 1

  weak_rsa_key="$test_root/weak-rsa.pem"
  openssl genpkey \
    -algorithm RSA \
    -pkeyopt rsa_keygen_bits:1024 \
    -out "$weak_rsa_key" 2>/dev/null
  chmod 0600 "$weak_rsa_key"
  if sign_checksum_with_validated_key \
    "$weak_rsa_key" \
    "$current_uid" \
    "$checksum" \
    "$test_root/weak-rsa.sig" \
    "$test_root/weak-rsa.pub.pem" \
    "$test_root/weak-rsa.mode" 2>/dev/null
  then
    printf 'release self-test accepted a weak RSA signing key\n' >&2
    return 1
  fi

  ed25519_key="$test_root/ed25519.pem"
  signature="$test_root/ed25519.sig"
  public_key="$test_root/ed25519.pub.pem"
  openssl genpkey -algorithm ED25519 -out "$ed25519_key"
  chmod 0400 "$ed25519_key"
  validate_signing_key "$ed25519_key" "$current_uid"
  signature_mode_file="$test_root/ed25519.mode"
  sign_checksum_with_validated_key \
    "$ed25519_key" \
    "$current_uid" \
    "$checksum" \
    "$signature" \
    "$public_key" \
    "$signature_mode_file"
  signature_mode="$(<"$signature_mode_file")"
  rm -f -- "$signature_mode_file"
  [[ "$signature_mode" == "EdDSA raw message" ]] || return 1
  if sign_checksum_with_validated_key \
    "$ed25519_key" \
    "$current_uid" \
    "$checksum" \
    "$test_root/missing/signature" \
    "$test_root/failed-signature.pub.pem" \
    "$test_root/failed-signature.mode" 2>/dev/null
  then
    printf 'release self-test masked a signature-output failure\n' >&2
    return 1
  fi
  if sign_checksum_with_validated_key \
    "$ed25519_key" \
    "$current_uid" \
    "$checksum" \
    "$test_root/failed-mode.sig" \
    "$test_root/failed-mode.pub.pem" \
    "$test_root" 2>/dev/null
  then
    printf 'release self-test masked a signature-mode write failure\n' >&2
    return 1
  fi

  ed448_key="$test_root/ed448.pem"
  openssl genpkey -algorithm ED448 -out "$ed448_key"
  chmod 0400 "$ed448_key"
  signature_mode_file="$test_root/ed448.mode"
  sign_checksum_with_validated_key \
    "$ed448_key" \
    "$current_uid" \
    "$checksum" \
    "$test_root/ed448.sig" \
    "$test_root/ed448.pub.pem" \
    "$signature_mode_file"
  signature_mode="$(<"$signature_mode_file")"
  rm -f -- "$signature_mode_file"
  [[ "$signature_mode" == "EdDSA raw message" ]] || return 1

  ec_key="$test_root/ec-p256.pem"
  openssl genpkey \
    -algorithm EC \
    -pkeyopt ec_paramgen_curve:prime256v1 \
    -out "$ec_key"
  chmod 0400 "$ec_key"
  signature_mode_file="$test_root/ec-p256.mode"
  sign_checksum_with_validated_key \
    "$ec_key" \
    "$current_uid" \
    "$checksum" \
    "$test_root/ec-p256.sig" \
    "$test_root/ec-p256.pub.pem" \
    "$signature_mode_file"
  signature_mode="$(<"$signature_mode_file")"
  rm -f -- "$signature_mode_file"
  [[ "$signature_mode" == "SHA-256 digest" ]] || return 1

  unapproved_ec_key="$test_root/ec-secp256k1.pem"
  openssl genpkey \
    -algorithm EC \
    -pkeyopt ec_paramgen_curve:secp256k1 \
    -out "$unapproved_ec_key"
  chmod 0400 "$unapproved_ec_key"
  if sign_checksum_with_validated_key \
    "$unapproved_ec_key" \
    "$current_uid" \
    "$checksum" \
    "$test_root/ec-secp256k1.sig" \
    "$test_root/ec-secp256k1.pub.pem" \
    "$test_root/ec-secp256k1.mode" 2>/dev/null
  then
    printf 'release self-test accepted an unapproved EC curve\n' >&2
    return 1
  fi

  dsa_parameters="$test_root/dsa-parameters.pem"
  dsa_key="$test_root/dsa.pem"
  openssl genpkey \
    -genparam \
    -algorithm DSA \
    -pkeyopt dsa_paramgen_bits:1024 \
    -out "$dsa_parameters" 2>/dev/null
  openssl genpkey \
    -paramfile "$dsa_parameters" \
    -out "$dsa_key" 2>/dev/null
  chmod 0400 "$dsa_key"
  if sign_checksum_with_validated_key \
    "$dsa_key" \
    "$current_uid" \
    "$checksum" \
    "$test_root/dsa.sig" \
    "$test_root/dsa.pub.pem" \
    "$test_root/dsa.mode" 2>/dev/null
  then
    printf 'release self-test accepted a DSA signing key\n' >&2
    return 1
  fi

  unknown_key="$test_root/x25519.pem"
  openssl genpkey -algorithm X25519 -out "$unknown_key"
  chmod 0400 "$unknown_key"
  if sign_checksum_with_validated_key \
    "$unknown_key" \
    "$current_uid" \
    "$checksum" \
    "$test_root/x25519.sig" \
    "$test_root/x25519.pub.pem" \
    "$test_root/x25519.mode" 2>/dev/null
  then
    printf 'release self-test accepted an unknown signing-key algorithm\n' >&2
    return 1
  fi

  test_repository="$test_root/source-repository"
  test_git_template="$test_root/source-template"
  install -d -m 0700 "$test_git_template"
  run_git_isolated init \
    --quiet \
    --template="$test_git_template" \
    "$test_repository"
  printf 'original\n' > "$test_repository/tracked.txt"
  printf '[package]\nname = "release-self-test"\nversion = "0.0.0-test"\n' \
    > "$test_repository/Cargo.toml"
  run_source_git "$test_repository" add -- Cargo.toml tracked.txt
  run_source_git "$test_repository" \
    -c user.name=release-self-test \
    -c user.email=release-self-test.invalid \
    -c commit.gpgSign=false \
    commit --quiet --message=original
  original_commit="$(
    run_source_git "$test_repository" rev-parse --verify "HEAD^{commit}"
  )"
  printf 'replacement\n' > "$test_repository/tracked.txt"
  run_source_git "$test_repository" add -- tracked.txt
  run_source_git "$test_repository" \
    -c user.name=release-self-test \
    -c user.email=release-self-test.invalid \
    -c commit.gpgSign=false \
    commit --quiet --message=replacement
  replacement_commit="$(
    run_source_git "$test_repository" rev-parse --verify "HEAD^{commit}"
  )"
  run_source_git "$test_repository" \
    checkout --quiet --detach "$original_commit"
  run_source_git "$test_repository" tag v0.0.0-test "$original_commit"
  validate_release_source_state \
    "$test_repository" \
    "$original_commit" \
    v0.0.0-test \
    0.0.0-test
  if validate_release_source_state \
    "$test_repository" \
    "$original_commit" \
    v0.0.0-test \
    9.9.9 2>/dev/null
  then
    printf 'release self-test accepted a mismatched Cargo version\n' >&2
    return 1
  fi
  printf 'dirty\n' > "$test_repository/tracked.txt"
  if validate_release_source_state \
    "$test_repository" \
    "$original_commit" \
    v0.0.0-test \
    0.0.0-test 2>/dev/null
  then
    printf 'release self-test accepted a dirty worktree\n' >&2
    return 1
  fi
  printf 'original\n' > "$test_repository/tracked.txt"

  run_source_git "$test_repository" \
    replace "$original_commit" "$replacement_commit"
  if validate_source_git_metadata "$test_repository" 2>/dev/null; then
    printf 'release self-test accepted refs/replace metadata\n' >&2
    return 1
  fi
  run_source_git "$test_repository" replace -d "$original_commit" >/dev/null

  repository_git_directory="$(
    run_source_git "$test_repository" \
      rev-parse --path-format=absolute --git-dir
  )"
  install -d -m 0700 "$repository_git_directory/info"
  printf 'tracked.txt export-ignore\n' \
    > "$repository_git_directory/info/attributes"
  if validate_source_git_metadata "$test_repository" 2>/dev/null; then
    printf 'release self-test accepted info/attributes metadata\n' >&2
    return 1
  fi
  rm -f -- "$repository_git_directory/info/attributes"
  validate_source_git_metadata "$test_repository"

  snapshot_directory="$test_root/source-snapshot.git"
  snapshot_template="$test_root/snapshot-template"
  source_sha="$original_commit"
  initialize_source_snapshot \
    "$test_repository" \
    "$snapshot_directory" \
    "$snapshot_template" \
    "$source_sha"

  # Mutate both repository-local mechanisms after the isolated object view has
  # been created. The snapshot archive must remain byte-stable and must still
  # materialize the original commit tree.
  run_source_git "$test_repository" \
    replace "$original_commit" "$replacement_commit"
  printf 'tracked.txt export-ignore\n' \
    > "$repository_git_directory/info/attributes"
  first_source_archive="$test_root/source-first.tar"
  second_source_archive="$test_root/source-second.tar"
  first_source_tree="$test_root/source-first"
  second_source_tree="$test_root/source-second"
  install -d -m 0700 "$first_source_tree" "$second_source_tree"
  create_and_verify_source_archive \
    "$first_source_archive" \
    "$first_source_tree" \
    "$test_root/source-first.index" \
    "$test_root/source-first.untracked"
  create_and_verify_source_archive \
    "$second_source_archive" \
    "$second_source_tree" \
    "$test_root/source-second.index" \
    "$test_root/source-second.untracked"
  [[ "$(<"$first_source_tree/tracked.txt")" == "original" ]] || {
    printf 'isolated Git archive followed a replace object\n' >&2
    return 1
  }
  [[ "$(<"$second_source_tree/tracked.txt")" == "original" ]] || {
    printf 'second isolated Git archive followed a replace object\n' >&2
    return 1
  }
  first_archive_checksum="$(sha256sum "$first_source_archive")"
  first_archive_checksum="${first_archive_checksum%% *}"
  second_archive_checksum="$(sha256sum "$second_source_archive")"
  second_archive_checksum="${second_archive_checksum%% *}"
  [[ "$first_archive_checksum" == "$second_archive_checksum" ]] || {
    printf 'isolated Git archives were not reproducible\n' >&2
    return 1
  }
  verify_quality_source_after_gate \
    "$first_source_tree" \
    "$test_root/quality.index" \
    "$test_root/quality.untracked"
  printf 'changed by quality gate\n' > "$first_source_tree/tracked.txt"
  if verify_quality_source_after_gate \
    "$first_source_tree" \
    "$test_root/quality.index" \
    "$test_root/quality.untracked" 2>/dev/null
  then
    printf 'release self-test missed a quality-gate source mutation\n' >&2
    return 1
  fi
  printf 'original\n' > "$first_source_tree/tracked.txt"
  printf 'unexpected\n' > "$first_source_tree/unexpected.txt"
  if verify_quality_source_after_gate \
    "$first_source_tree" \
    "$test_root/quality.index" \
    "$test_root/quality.untracked" 2>/dev/null
  then
    printf 'release self-test missed an unexpected quality-gate path\n' >&2
    return 1
  fi
  rm -f -- "$first_source_tree/unexpected.txt"
  if validate_source_git_metadata "$test_repository" 2>/dev/null; then
    printf 'release self-test missed post-snapshot Git metadata changes\n' >&2
    return 1
  fi
  run_source_git "$test_repository" replace -d "$original_commit" >/dev/null
  rm -f -- "$repository_git_directory/info/attributes"

  verifier_temporary_directory="$test_root/verifier-temporary"
  install -d -m 0700 "$verifier_temporary_directory"
  TMPDIR="$verifier_temporary_directory" \
    validate_source_tree_entries "$test_repository" "$original_commit"
  verifier_leak="$(
    find -P "$verifier_temporary_directory" -mindepth 1 -print -quit
  )" || {
    printf 'release self-test could not inspect verifier cleanup\n' >&2
    return 1
  }
  [[ -z "$verifier_leak" ]] || {
    printf 'release self-test leaked a successful verifier stream\n' >&2
    return 1
  }

  partial_git="$test_root/partial-git"
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    "printf '100644 blob 0123456789abcdef0123456789abcdef01234567\\ttracked.txt\\0'" \
    "printf 'simulated partial Git listing failure\\n' >&2" \
    'exit 73' > "$partial_git"
  chmod 0700 "$partial_git"
  if (
    TMPDIR="$verifier_temporary_directory"
    git_command="$partial_git"
    validate_source_tree_entries "$test_repository" "$original_commit"
  ) 2>/dev/null
  then
    printf 'release self-test masked a partial source ls-tree failure\n' >&2
    return 1
  fi
  if (
    TMPDIR="$verifier_temporary_directory"
    git_command="$partial_git"
    validate_snapshot_tree_entries "$original_commit"
  ) 2>/dev/null
  then
    printf 'release self-test masked a partial snapshot ls-tree failure\n' >&2
    return 1
  fi
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    "printf '100644 blob 0123456789abcdef0123456789abcdef01234567\\ttruncated.txt'" \
    'exit 0' > "$partial_git"
  chmod 0700 "$partial_git"
  if (
    TMPDIR="$verifier_temporary_directory"
    git_command="$partial_git"
    validate_source_tree_entries "$test_repository" "$original_commit"
  ) 2>/dev/null
  then
    printf 'release self-test accepted a truncated source ls-tree record\n' >&2
    return 1
  fi
  if (
    TMPDIR="$verifier_temporary_directory"
    git_command="$partial_git"
    validate_snapshot_tree_entries "$original_commit"
  ) 2>/dev/null
  then
    printf 'release self-test accepted a truncated snapshot ls-tree record\n' >&2
    return 1
  fi

  partial_find_bin="$test_root/partial-find-bin"
  partial_find="$partial_find_bin/find"
  install -d -m 0700 "$partial_find_bin"
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'if [[ -f SHA256SUMS ]]; then' \
    '  while IFS= read -r checksum_line; do' \
    '    package_file=${checksum_line#*  }' \
    "    printf '%s\\0' \"\$package_file\"" \
    '  done < SHA256SUMS' \
    'fi' \
    "printf 'simulated partial find failure\\n' >&2" \
    'exit 73' > "$partial_find"
  chmod 0700 "$partial_find"
  if (
    TMPDIR="$verifier_temporary_directory"
    PATH="$partial_find_bin:/usr/bin:/bin"
    hash -r
    validate_extracted_source_tree "$first_source_tree"
  ) 2>/dev/null
  then
    printf 'release self-test masked a partial extraction scan failure\n' >&2
    return 1
  fi
  if (
    TMPDIR="$verifier_temporary_directory"
    PATH="$partial_find_bin:/usr/bin:/bin"
    hash -r
    verify_release_package_checksum_coverage "$documentation_package"
  ) 2>/dev/null
  then
    printf 'release self-test masked a partial checksum traversal failure\n' >&2
    return 1
  fi
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'last_package_file=' \
    'while IFS= read -r checksum_line; do' \
    '  package_file=${checksum_line#*  }' \
    '  if [[ -n "$last_package_file" ]]; then' \
    "    printf '%s\\0' \"\$last_package_file\"" \
    '  fi' \
    '  last_package_file=$package_file' \
    'done < SHA256SUMS' \
    "printf '%s' \"\$last_package_file\"" \
    'exit 0' > "$partial_find"
  chmod 0700 "$partial_find"
  if (
    TMPDIR="$verifier_temporary_directory"
    PATH="$partial_find_bin:/usr/bin:/bin"
    hash -r
    verify_release_package_checksum_coverage "$documentation_package"
  ) 2>/dev/null
  then
    printf 'release self-test accepted a truncated checksum traversal record\n' >&2
    return 1
  fi
  verifier_leak="$(
    find -P "$verifier_temporary_directory" -mindepth 1 -print -quit
  )" || {
    printf 'release self-test could not re-inspect verifier cleanup\n' >&2
    return 1
  }
  [[ -z "$verifier_leak" ]] || {
    printf 'release self-test leaked a failed verifier stream\n' >&2
    return 1
  }

  for unsafe_case in \
    'README.md|../outside-release-tree' \
    'docs/manual.md|/etc/passwd' \
    'build.rs|../../outside-build-input'
  do
    IFS='|' read -r unsafe_path unsafe_target <<< "$unsafe_case"
    if [[ -n "$previous_unsafe_path" ]]; then
      run_source_git "$test_repository" rm -q -f -- "$previous_unsafe_path"
    fi
    unsafe_parent="${unsafe_path%/*}"
    if [[ "$unsafe_parent" != "$unsafe_path" ]]; then
      mkdir -p -- "$test_repository/$unsafe_parent"
    fi
    ln -s -- "$unsafe_target" "$test_repository/$unsafe_path"
    run_source_git "$test_repository" add -- "$unsafe_path"
    run_source_git "$test_repository" \
      -c user.name=release-self-test \
      -c user.email=release-self-test.invalid \
      -c commit.gpgSign=false \
      commit --quiet --message="unsafe symlink $unsafe_path"
    unsafe_commit="$(
      run_source_git "$test_repository" rev-parse --verify "HEAD^{commit}"
    )"
    if validate_snapshot_tree_entries "$unsafe_commit" 2>/dev/null; then
      printf 'release self-test accepted tracked symlink %s\n' \
        "$unsafe_path" >&2
      return 1
    fi
    if validate_source_tree_entries \
      "$test_repository" \
      "$unsafe_commit" 2>/dev/null
    then
      printf 'release preflight accepted tracked symlink %s\n' \
        "$unsafe_path" >&2
      return 1
    fi
    previous_unsafe_path="$unsafe_path"
  done

  run_source_git "$test_repository" rm -q -f -- "$previous_unsafe_path"
  run_source_git "$test_repository" update-index \
    --add \
    --cacheinfo "160000,$original_commit,submodule"
  run_source_git "$test_repository" \
    -c user.name=release-self-test \
    -c user.email=release-self-test.invalid \
    -c commit.gpgSign=false \
    commit --quiet --message='unsafe submodule'
  unsafe_commit="$(
    run_source_git "$test_repository" rev-parse --verify "HEAD^{commit}"
  )"
  if validate_snapshot_tree_entries "$unsafe_commit" 2>/dev/null; then
    printf 'release self-test accepted a submodule entry\n' >&2
    return 1
  fi
  if validate_source_tree_entries \
    "$test_repository" \
    "$unsafe_commit" 2>/dev/null
  then
    printf 'release preflight accepted a submodule entry\n' >&2
    return 1
  fi

  extracted_safety_test="$test_root/extracted-safety"
  install -d -m 0700 "$extracted_safety_test"
  ln -s -- /etc/passwd "$extracted_safety_test/link"
  if validate_extracted_source_tree "$extracted_safety_test" 2>/dev/null; then
    printf 'release self-test accepted an extracted symbolic link\n' >&2
    return 1
  fi
  rm -f -- "$extracted_safety_test/link"
  mkfifo "$extracted_safety_test/fifo"
  if validate_extracted_source_tree "$extracted_safety_test" 2>/dev/null; then
    printf 'release self-test accepted an extracted special file\n' >&2
    return 1
  fi

  notice_test_root="$test_root/notice-root"
  notice_test_file="$notice_test_root/share/doc/rust/COPYRIGHT-library.html"
  notice_test_outside="$test_root/outside-notice"
  notice_test_link="$notice_test_root/share/doc/rust/symlink.html"
  install -d -m 0700 "${notice_test_file%/*}"
  printf 'reviewed standard-library notice\n' > "$notice_test_file"
  printf 'outside notice\n' > "$notice_test_outside"
  notice_test_checksum="$(sha256sum < "$notice_test_file")"
  notice_test_checksum="${notice_test_checksum%% *}"
  [[ "$(
    validate_contained_notice_file \
      "$notice_test_root" \
      "$notice_test_file" \
      "$notice_test_checksum"
  )" == "$(realpath -e -- "$notice_test_file")" ]] || {
    printf 'release self-test rejected a valid contained notice\n' >&2
    return 1
  }
  if validate_contained_notice_file \
    "$notice_test_root" \
    "$notice_test_file" \
    "${notice_test_checksum%?}0" >/dev/null 2>&1
  then
    printf 'release self-test accepted a notice checksum mismatch\n' >&2
    return 1
  fi
  if validate_contained_notice_file \
    "$notice_test_root" \
    "$notice_test_outside" \
    "$(sha256sum < "$notice_test_outside")" >/dev/null 2>&1
  then
    printf 'release self-test accepted a notice outside its root\n' >&2
    return 1
  fi
  ln -s -- "$notice_test_file" "$notice_test_link"
  if validate_contained_notice_file \
    "$notice_test_root" \
    "$notice_test_link" \
    "$notice_test_checksum" >/dev/null 2>&1
  then
    printf 'release self-test accepted a symbolic-link notice\n' >&2
    return 1
  fi
  [[ "$(expected_rust_library_notice_sha256 1.97.1)" == \
    '0a65bb747c49c7bb816cbc7188319bd6e4e8d08091c1190b8a3c0971c47968ed' ]] || {
    printf 'release self-test found the wrong pinned Rust notice digest\n' >&2
    return 1
  }
  if expected_rust_library_notice_sha256 0.0.0 >/dev/null 2>&1; then
    printf 'release self-test accepted an unreviewed Rust toolchain notice\n' >&2
    return 1
  fi

  run_node_entrypoint \
    "$node_command" \
    "$project_dir/scripts/normalize-sbom.mjs" \
    --self-test
  run_node_entrypoint \
    "$node_command" \
    "$project_dir/scripts/generate-third-party-notices.mjs" \
    --self-test
  run_node_entrypoint "$node_command" "$project_dir/scripts/seed-npm-cache.mjs" \
    --self-test \
    "$npm_command"

  exec {test_lock_fd}>&-
  rm -rf --one-file-system -- "$test_root"
  cleanup_path=""
  printf 'atomic release-directory publication self-test passed\n'
}

signing_key_argument=""
output_dir_argument="$project_dir/dist"
output_dir_was_set=false
self_test=false
required_cargo_cyclonedx_version="0.5.9"
required_cargo_audit_version="0.22.2"
rustsec_advisory_database_url="https://github.com/RustSec/advisory-db.git"
rustsec_advisory_database_maximum_age_seconds=604800
rustsec_advisory_database_maximum_future_skew_seconds=300
while (($# > 0)); do
  case "$1" in
    --signing-key)
      (($# >= 2)) || { usage >&2; exit 2; }
      signing_key_argument="$2"
      shift 2
      ;;
    --output-dir)
      (($# >= 2)) || { usage >&2; exit 2; }
      output_dir_argument="$2"
      output_dir_was_set=true
      shift 2
      ;;
    --self-test)
      self_test=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'Unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

for command_name in \
  cargo \
  chmod \
  date \
  env \
  find \
  flock \
  git \
  grep \
  gzip \
  id \
  install \
  ln \
  mkdir \
  mkfifo \
  mktemp \
  mv \
  node \
  npm \
  openssl \
  realpath \
  rm \
  rustc \
  sed \
  sha256sum \
  sort \
  stat \
  sync \
  tar \
  touch \
  true
do
  require_command "$command_name"
done
git_command="$(command -v git)"
node_command="$(command -v node)"
npm_command="$(command -v npm)"
mv_help="$(LC_ALL=C mv --help 2>&1)"
if [[ "$mv_help" != *"--no-copy"* || \
  "$mv_help" != *"--no-target-directory"* || \
  "$mv_help" != *"UPDATE={all,none"* ]]
then
  printf '%s\n' \
    'GNU mv with --no-copy, --no-target-directory, and --update=none support is required for atomic publication.' >&2
  exit 1
fi
unset mv_help

if [[ "$self_test" == true ]]; then
  if [[ -n "$signing_key_argument" || "$output_dir_was_set" == true ]]; then
    printf '%s\n' \
      '--self-test cannot be combined with --signing-key or --output-dir' >&2
    exit 2
  fi
  run_publication_self_test
  exit 0
fi

# A formal release must run the warning-level ShellCheck gate inside its
# isolated quality environment. The lightweight publication self-test above
# deliberately remains usable on offline development hosts.
require_shellcheck_version
shellcheck_command="$(realpath -- "$(command -v shellcheck)")"
[[ -x "$shellcheck_command" ]] || {
  printf 'Resolved ShellCheck command is not executable: %s\n' \
    "$shellcheck_command" >&2
  exit 1
}

[[ -n "$signing_key_argument" ]] || {
  printf 'A readable PEM signing key is required.\n' >&2
  exit 2
}
current_uid="$(id -u)"

# Stage secrets and build intermediates privately. Public file and directory
# modes are assigned explicitly below.
umask 077

validate_source_git_metadata "$project_dir"
[[ -z "$(
  run_source_git "$project_dir" \
    status --porcelain --untracked-files=all
)" ]] || {
  printf 'Refusing to package a dirty worktree.\n' >&2
  exit 1
}

source_sha="$(
  run_source_git "$project_dir" rev-parse --verify "HEAD^{commit}"
)"
version="$(
  run_source_git "$project_dir" show "$source_sha:Cargo.toml" |
    sed -n 's/^version = "\([^"]*\)"/\1/p'
)"
[[ -n "$version" && "$version" != *$'\n'* ]] || {
  printf 'Unable to determine one package version from source commit %s.\n' \
    "$source_sha" >&2
  exit 1
}
release_tag="v$version"
tag_sha="$(
  run_source_git "$project_dir" \
    rev-parse --verify "refs/tags/$release_tag^{commit}" 2>/dev/null
)" || {
  printf 'Required release tag does not exist: %s\n' "$release_tag" >&2
  exit 1
}
[[ "$tag_sha" == "$source_sha" ]] || {
  printf 'Release tag %s does not point to HEAD %s.\n' \
    "$release_tag" \
    "$source_sha" >&2
  exit 1
}

validate_source_tree_entries "$project_dir" "$source_sha"

required_rust_version="$(
  run_source_git "$project_dir" show "$source_sha:rust-toolchain.toml" |
    sed -n 's/^channel = "\([^"]*\)"/\1/p'
)"
[[ -n "$required_rust_version" && "$required_rust_version" != *$'\n'* ]] || {
  printf 'Unable to determine the pinned Rust toolchain.\n' >&2
  exit 1
}
cargo_command="$(command -v cargo)"
rustc_command="$(command -v rustc)"
rustc_version="$(
  env RUSTUP_TOOLCHAIN="$required_rust_version" \
    "$rustc_command" --version
)"
cargo_version="$(
  env RUSTUP_TOOLCHAIN="$required_rust_version" \
    "$cargo_command" --version
)"
[[ "$rustc_version" == "rustc $required_rust_version "* ]] || {
  printf 'Pinned rustc %s is required; found: %s\n' \
    "$required_rust_version" \
    "$rustc_version" >&2
  exit 1
}
[[ "$cargo_version" == "cargo $required_rust_version "* ]] || {
  printf 'Pinned Cargo %s is required; found: %s\n' \
    "$required_rust_version" \
    "$cargo_version" >&2
  exit 1
}
rust_library_notice_checksum="$(
  expected_rust_library_notice_sha256 "$required_rust_version"
)"
rust_library_notice_source="$(
  locate_rust_library_notice \
    "$required_rust_version" \
    "$rustc_command" \
    "$rust_library_notice_checksum"
)"
cargo_cyclonedx_version="$(
  env RUSTUP_TOOLCHAIN="$required_rust_version" \
    "$cargo_command" cyclonedx --version 2>/dev/null
)" || {
  printf 'Required Cargo subcommand is unavailable: cargo cyclonedx\n' >&2
  exit 1
}
cargo_audit_version="$(
  env RUSTUP_TOOLCHAIN="$required_rust_version" \
    "$cargo_command" audit --version 2>/dev/null
)" || {
  printf 'Required Cargo subcommand is unavailable: cargo audit\n' >&2
  exit 1
}
validate_cargo_audit_version_line \
  "$cargo_audit_version" \
  "$required_cargo_audit_version" || {
  printf 'Required cargo-audit version is %s; found: %s\n' \
    "$required_cargo_audit_version" \
    "$cargo_audit_version" >&2
  exit 1
}
expected_cargo_cyclonedx_version="cargo-cyclonedx-cyclonedx $required_cargo_cyclonedx_version"
[[ "$cargo_cyclonedx_version" == "$expected_cargo_cyclonedx_version" ]] || {
  printf 'Required cargo-cyclonedx version is %s; found: %s\n' \
    "$required_cargo_cyclonedx_version" \
    "$cargo_cyclonedx_version" >&2
  exit 1
}

host_target="$(
  env RUSTUP_TOOLCHAIN="$required_rust_version" \
    "$rustc_command" -vV |
    sed -n 's/^host: //p'
)"
release_epoch="${SOURCE_DATE_EPOCH:-$(
  run_source_git "$project_dir" show -s --format=%ct "$source_sha"
)}"
[[ -n "$host_target" && "$host_target" != *$'\n'* ]] || {
  printf 'Unable to determine the Rust host target.\n' >&2
  exit 1
}
[[ "$release_epoch" =~ ^[0-9]+$ ]] || {
  printf 'SOURCE_DATE_EPOCH must be a non-negative integer.\n' >&2
  exit 1
}

output_dir="$(canonicalize_output_directory "$output_dir_argument")"
validate_output_directory "$output_dir" "$current_uid"
output_identity="$(stat -Lc '%d:%i' -- "$output_dir")"
exec {release_lock_fd}<"$output_dir"
flock --exclusive "$release_lock_fd"
locked_output_directory="/proc/$packager_pid/fd/$release_lock_fd"
validate_output_directory "$output_dir" "$current_uid"
[[ "$(stat -Lc '%d:%i' -- "$output_dir")" == "$output_identity" ]] || {
  printf 'Release output directory changed while acquiring its lock.\n' >&2
  exit 1
}
[[ "$(stat -Lc '%d:%i' -- "$locked_output_directory")" == "$output_identity" ]] || {
  printf 'Release output lock does not refer to the validated directory.\n' >&2
  exit 1
}

# From this point onward every stage mutation is rooted below the locked
# directory fd. The public output string is used only for identity checks and
# the final human-readable path.
release_stage="$(
  mktemp \
    -d \
    --tmpdir="$locked_output_directory" \
    .dufs-release-stage.XXXXXXXX
)"
chmod 0700 "$release_stage"
release_stage_basename="${release_stage##*/}"
release_stage_via_lock="$locked_output_directory/$release_stage_basename"
release_stage="$release_stage_via_lock"
release_stage_physical_at_creation="$(realpath -e -- "$release_stage")"
cleanup_path="$release_stage_via_lock"
cleanup_parent="$locked_output_directory"
cleanup_prefix=".dufs-release-stage."

quality_source="$release_stage/quality-source"
quality_target_dir="$release_stage/quality-cargo-target"
quality_isolated_home="$release_stage/quality-home"
quality_isolated_cargo_home="$release_stage/quality-cargo-home"
quality_tmp_dir="$release_stage/quality-tmp"
quality_tmp_dir_physical="$release_stage_physical_at_creation/quality-tmp"
quality_vendor="$release_stage/quality-vendor"
quality_npm_cache="$release_stage/quality-npm-cache"
quality_source_archive="$release_stage/quality-source.tar"
quality_audit_db="$quality_isolated_cargo_home/advisory-db"
release_build_source="$release_stage/build-source"
release_package_source="$release_stage/package-source"
release_target_dir="$release_stage/cargo-target"
release_isolated_home="$release_stage/home"
release_isolated_cargo_home="$release_stage/cargo-home"
release_tmp_dir="$release_stage/tmp"
release_snapshot_git="$release_stage/source-snapshot.git"
release_git_template="$release_stage/git-template"
release_rust_library_notice="$release_stage/RUST-STANDARD-LIBRARY-COPYRIGHT.html"
first_source_archive="$release_stage/source-first.tar"
second_source_archive="$release_stage/source-second.tar"
install -d -m 0700 \
  "$quality_source" \
  "$quality_target_dir" \
  "$quality_isolated_home" \
  "$quality_isolated_cargo_home" \
  "$quality_tmp_dir" \
  "$quality_vendor" \
  "$quality_npm_cache" \
  "$release_build_source" \
  "$release_package_source" \
  "$release_isolated_home" \
  "$release_isolated_cargo_home" \
  "$release_tmp_dir"
install -m 0600 /dev/null "$quality_isolated_home/npm-userconfig"
install -m 0600 /dev/null "$quality_isolated_home/npm-globalconfig"
quality_tmp_metadata="$(stat -Lc '%u:%a:%d:%i' -- "$quality_tmp_dir")"
[[ "$quality_tmp_metadata" == "$current_uid:700:"* ]] || {
  printf 'Quality-gate temporary directory is not private.\n' >&2
  exit 1
}
validate_private_directory_binding \
  "$quality_tmp_dir" \
  "$quality_tmp_dir_physical" \
  "$quality_tmp_metadata" \
  'Quality-gate temporary directory'
initialize_source_snapshot \
  "$project_dir" \
  "$release_snapshot_git" \
  "$release_git_template" \
  "$source_sha"
create_and_verify_source_archive \
  "$quality_source_archive" \
  "$quality_source" \
  "$release_stage/quality-source.index" \
  "$release_stage/quality-source.untracked"
rust_library_notice_source="$(
  locate_rust_library_notice \
    "$required_rust_version" \
    "$rustc_command" \
    "$rust_library_notice_checksum"
)"
install -m 0600 \
  "$rust_library_notice_source" \
  "$release_rust_library_notice"
validate_contained_notice_file \
  "$release_stage" \
  "$release_rust_library_notice" \
  "$rust_library_notice_checksum" >/dev/null

if [[ -n "${CARGO_HOME:-}" ]]; then
  host_cargo_home_candidate="$(resolve_invocation_path "$CARGO_HOME")"
elif [[ -n "${HOME:-}" ]]; then
  host_cargo_home_candidate="$HOME/.cargo"
else
  printf 'HOME or CARGO_HOME is required to vendor locked dependencies.\n' >&2
  exit 1
fi
mkdir -p -- "$host_cargo_home_candidate"
host_cargo_home="$(
  cd -P -- "$host_cargo_home_candidate"
  pwd -P
)"
if [[ -n "${RUSTUP_HOME:-}" ]]; then
  host_rustup_home_candidate="$(resolve_invocation_path "$RUSTUP_HOME")"
elif [[ -n "${HOME:-}" ]]; then
  host_rustup_home_candidate="$HOME/.rustup"
else
  host_rustup_home_candidate=""
fi
host_rustup_home=""
if [[ -n "$host_rustup_home_candidate" && -d "$host_rustup_home_candidate" ]]; then
  host_rustup_home="$(
    cd -P -- "$host_rustup_home_candidate"
    pwd -P
  )"
fi

host_npm_cache_candidate="$("$npm_command" config get cache)"
[[ -n "$host_npm_cache_candidate" && \
  "$host_npm_cache_candidate" != *$'\n'* ]] || {
  printf 'npm returned an invalid cache path.\n' >&2
  exit 1
}
case "$host_npm_cache_candidate" in
  /*) ;;
  *) host_npm_cache_candidate="$(resolve_invocation_path "$host_npm_cache_candidate")" ;;
esac
if [[ -d "$host_npm_cache_candidate" ]]; then
  host_npm_cache="$(
    cd -P -- "$host_npm_cache_candidate"
    pwd -P
  )"
else
  host_npm_cache="$release_stage/empty-host-npm-cache"
  install -d -m 0700 "$host_npm_cache"
fi

host_browser_cache_candidate=""
if [[ -n "${PLAYWRIGHT_BROWSERS_PATH:-}" && \
  "${PLAYWRIGHT_BROWSERS_PATH:-}" != "0" ]]
then
  host_browser_cache_candidate="$PLAYWRIGHT_BROWSERS_PATH"
elif [[ -n "${HOME:-}" ]]; then
  host_browser_cache_candidate="$HOME/.cache/ms-playwright"
fi
host_browser_cache=""
if [[ -n "$host_browser_cache_candidate" ]]; then
  case "$host_browser_cache_candidate" in
    /*) ;;
    *)
      host_browser_cache_candidate="$(
        resolve_invocation_path "$host_browser_cache_candidate"
      )"
      ;;
  esac
  if [[ -d "$host_browser_cache_candidate" ]]; then
    host_browser_cache="$(
      cd -P -- "$host_browser_cache_candidate"
      pwd -P
    )"
  fi
fi

cargo_bin_dir="${cargo_command%/*}"
rustc_bin_dir="${rustc_command%/*}"
node_bin_dir="${node_command%/*}"
npm_bin_dir="${npm_command%/*}"
shellcheck_bin_dir="${shellcheck_command%/*}"
release_tool_path="$cargo_bin_dir:$rustc_bin_dir:/usr/local/bin:/usr/bin:/bin"
quality_tool_path="$cargo_bin_dir:$rustc_bin_dir:$node_bin_dir:$npm_bin_dir"
quality_tool_path+=":$shellcheck_bin_dir"
quality_tool_path+=":/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
vendor_environment=(
  env -i
  "CARGO_HOME=$host_cargo_home"
  "HOME=$release_isolated_home"
  "LANG=C"
  "LC_ALL=C"
  "PATH=$release_tool_path"
  "RUSTUP_TOOLCHAIN=$required_rust_version"
  "TMPDIR=$release_tmp_dir"
  "TZ=UTC"
)
if [[ -n "$host_rustup_home" ]]; then
  vendor_environment+=("RUSTUP_HOME=$host_rustup_home")
fi
for network_variable in \
  ALL_PROXY \
  HTTPS_PROXY \
  HTTP_PROXY \
  NO_PROXY \
  SSL_CERT_DIR \
  SSL_CERT_FILE
do
  if [[ -v "$network_variable" ]]; then
    vendor_environment+=("$network_variable=${!network_variable}")
  fi
done

audit_environment=(
  env -i
  "CARGO=$cargo_command"
  "CARGO_HOME=$quality_isolated_cargo_home"
  "GIT_ATTR_NOSYSTEM=1"
  "GIT_CEILING_DIRECTORIES=$release_stage_physical_at_creation"
  "GIT_CONFIG_GLOBAL=/dev/null"
  "GIT_CONFIG_NOSYSTEM=1"
  "GIT_CONFIG_SYSTEM=/dev/null"
  "GIT_NO_REPLACE_OBJECTS=1"
  "HOME=$quality_isolated_home"
  "LANG=C"
  "LC_ALL=C"
  "NO_COLOR=1"
  "PATH=$quality_tool_path"
  "RUSTUP_TOOLCHAIN=$required_rust_version"
  "TMPDIR=$quality_tmp_dir"
  "TZ=UTC"
)
if [[ -n "$host_rustup_home" ]]; then
  audit_environment+=("RUSTUP_HOME=$host_rustup_home")
fi
for network_variable in \
  ALL_PROXY \
  HTTPS_PROXY \
  HTTP_PROXY \
  NO_PROXY \
  SSL_CERT_DIR \
  SSL_CERT_FILE
do
  if [[ -v "$network_variable" ]]; then
    audit_environment+=("$network_variable=${!network_variable}")
  fi
done

host_audit_db="$host_cargo_home/advisory-db"
rustsec_advisory_db_revision=""
rustsec_advisory_db_fetch_epoch=""
rustsec_advisory_db_index_checksum=""
rustsec_advisory_db_config_checksum=""
advisory_database_current_epoch="$(date -u +%s)"
[[ "$advisory_database_current_epoch" =~ ^[0-9]{1,12}$ ]] || {
  printf 'Unable to determine the current epoch for RustSec freshness checks.\n' >&2
  exit 1
}
quality_audit_db_reused=false
if [[ -e "$host_audit_db" || -L "$host_audit_db" ]]; then
  host_audit_database_classification="$(
    classify_advisory_database_identity "$host_audit_db"
  )" || exit $?
  read -r \
    host_audit_database_status \
    host_audit_database_revision \
    host_audit_database_fetch_epoch <<< \
    "$host_audit_database_classification"
  case "$host_audit_database_status" in
    reusable)
      advisory_database_freshness_status=0
      validate_advisory_database_freshness \
        "$host_audit_database_fetch_epoch" \
        "$advisory_database_current_epoch" \
        "$rustsec_advisory_database_maximum_age_seconds" \
        "$rustsec_advisory_database_maximum_future_skew_seconds" || \
        advisory_database_freshness_status=$?
      case "$advisory_database_freshness_status" in
        0)
          if validate_advisory_database_state \
            "$host_audit_db" \
            "$host_audit_database_revision" \
            "$host_audit_database_fetch_epoch" >/dev/null
          then
            run_git_isolated clone \
              --quiet \
              --no-hardlinks \
              -- \
              "$host_audit_db" \
              "$quality_audit_db"
            run_advisory_database_git \
              "$quality_audit_db" \
              remote set-url origin "$rustsec_advisory_database_url"
            printf '%s\t\t%s\n' \
              "$host_audit_database_revision" \
              "$rustsec_advisory_database_url" > \
              "$quality_audit_db/.git/FETCH_HEAD"
            touch \
              --date="@$host_audit_database_fetch_epoch" \
              -- \
              "$quality_audit_db/.git/FETCH_HEAD"
            quality_audit_db_reused=true
          else
            printf '%s\n' \
              'The fresh host RustSec database failed full validation; the isolated release pre-audit will require a network refresh.' >&2
          fi
          ;;
        1)
          printf '%s\n' \
            'The reusable RustSec database is stale; the isolated release pre-audit will require a network refresh.'
          ;;
        *)
          printf 'RustSec advisory database has an invalid future fetch timestamp.\n' >&2
          exit 1
          ;;
      esac
      ;;
    unavailable)
      printf '%s\n' \
        'The host RustSec database is not reusable; the isolated release pre-audit will require a network refresh.' >&2
      ;;
    *)
      printf 'Internal error: invalid RustSec database classification.\n' >&2
      exit 1
      ;;
  esac
fi
if [[ "$quality_audit_db_reused" != true ]]; then
  audit_refresh_lockfile="$release_stage/audit-refresh-Cargo.lock"
  printf 'version = 4\n' > "$audit_refresh_lockfile"
  printf '%s\n' \
    'Refreshing the private RustSec database before any project or dependency code runs.'
  (
    cd "$quality_source"
    "${audit_environment[@]}" \
      "$cargo_command" audit \
      --db "$quality_audit_db" \
      --file "$audit_refresh_lockfile" \
      --no-yanked \
      --url "$rustsec_advisory_database_url"
  )
  rm -f -- "$audit_refresh_lockfile"
fi

quality_audit_database_state="$(
  validate_advisory_database_state "$quality_audit_db"
)" || exit $?
read -r \
  rustsec_advisory_db_revision \
  rustsec_advisory_db_fetch_epoch \
  rustsec_advisory_db_index_checksum \
  rustsec_advisory_db_config_checksum <<< \
  "$quality_audit_database_state"
advisory_database_current_epoch="$(date -u +%s)"
[[ "$advisory_database_current_epoch" =~ ^[0-9]{1,12}$ ]] || {
  printf 'Unable to refresh the current epoch for RustSec checks.\n' >&2
  exit 1
}
advisory_database_freshness_status=0
validate_advisory_database_freshness \
  "$rustsec_advisory_db_fetch_epoch" \
  "$advisory_database_current_epoch" \
  "$rustsec_advisory_database_maximum_age_seconds" \
  "$rustsec_advisory_database_maximum_future_skew_seconds" || \
  advisory_database_freshness_status=$?
case "$advisory_database_freshness_status" in
  0) ;;
  1)
    printf 'The private RustSec advisory database is stale after refresh.\n' >&2
    exit 1
    ;;
  *)
    printf 'The private RustSec advisory database has an invalid future timestamp.\n' >&2
    exit 1
    ;;
esac

printf 'Running the sealed RustSec pre-audit for commit %s.\n' "$source_sha"
(
  cd "$quality_source"
  "${audit_environment[@]}" \
    CARGO_NET_OFFLINE=true \
    "$cargo_command" audit \
    --db "$quality_audit_db" \
    --no-fetch \
    --no-yanked
)
printf 'Fetching the locked release graph into the private Cargo index.\n'
(
  cd "$release_stage"
  "${audit_environment[@]}" \
    "$cargo_command" fetch \
    --locked \
    --manifest-path "$quality_source/Cargo.toml"
)
printf 'Checking the locked release graph for yanked crates.\n'
(
  cd "$quality_source"
  "${audit_environment[@]}" \
    "$cargo_command" audit \
    --db "$quality_audit_db" \
    --no-fetch \
    --deny yanked
)
validate_advisory_database_state \
  "$quality_audit_db" \
  "$rustsec_advisory_db_revision" \
  "$rustsec_advisory_db_fetch_epoch" \
  "$rustsec_advisory_db_index_checksum" \
  "$rustsec_advisory_db_config_checksum" >/dev/null

quality_vendor_config="$release_stage/quality-vendor-config.toml"
(
  cd "$quality_source"
  "${vendor_environment[@]}" \
    "$cargo_command" vendor \
    --locked \
    --versioned-dirs \
    "$quality_vendor" > "$quality_vendor_config"
)
[[ -s "$quality_vendor_config" ]] || {
  printf 'Cargo produced an empty quality-gate vendor configuration.\n' >&2
  exit 1
}
validate_extracted_source_tree "$quality_vendor"
install -m 0600 \
  "$quality_vendor_config" \
  "$quality_isolated_cargo_home/config.toml"

run_node_entrypoint "$node_command" \
  "$quality_source/scripts/seed-npm-cache.mjs" \
  "$quality_source/package-lock.json" \
  "$host_npm_cache" \
  "$quality_npm_cache" \
  "$npm_command"

quality_environment=(
  env -i
  "CARGO=$cargo_command"
  "CARGO_HOME=$quality_isolated_cargo_home"
  "CARGO_INCREMENTAL=0"
  "CARGO_NET_OFFLINE=true"
  "CARGO_TARGET_DIR=$quality_target_dir"
  "CI=1"
  "DUFS_BUILD_GIT_SHA=$source_sha"
  "DUFS_ISOLATED_QUALITY_GATE=1"
  "DUFS_QUALITY_AUDIT_DB=$quality_audit_db"
  "DUFS_REQUIRE_SHELLCHECK=1"
  "GIT_ATTR_NOSYSTEM=1"
  "GIT_CEILING_DIRECTORIES=$release_stage_physical_at_creation"
  "GIT_CONFIG_GLOBAL=/dev/null"
  "GIT_CONFIG_NOSYSTEM=1"
  "GIT_CONFIG_SYSTEM=/dev/null"
  "GIT_NO_REPLACE_OBJECTS=1"
  "HOME=$quality_isolated_home"
  "LANG=C"
  "LC_ALL=C"
  "NO_COLOR=1"
  "PATH=$quality_tool_path"
  "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1"
  "RUSTC=$rustc_command"
  "RUSTUP_TOOLCHAIN=$required_rust_version"
  "SOURCE_DATE_EPOCH=$release_epoch"
  "TMPDIR=$quality_tmp_dir_physical"
  "TZ=UTC"
  "XDG_CACHE_HOME=$quality_isolated_home/.cache"
  "XDG_CONFIG_HOME=$quality_isolated_home/.config"
  "XDG_DATA_HOME=$quality_isolated_home/.local/share"
  "npm_config_cache=$quality_npm_cache"
  "npm_config_fund=false"
  "npm_config_globalconfig=$quality_isolated_home/npm-globalconfig"
  "npm_config_ignore_scripts=true"
  "npm_config_prefer_offline=true"
  "npm_config_strict_ssl=true"
  "npm_config_update_notifier=false"
  "npm_config_userconfig=$quality_isolated_home/npm-userconfig"
)
if [[ -n "$host_rustup_home" ]]; then
  quality_environment+=("RUSTUP_HOME=$host_rustup_home")
fi
if [[ -n "$host_browser_cache" ]]; then
  quality_environment+=("PLAYWRIGHT_BROWSERS_PATH=$host_browser_cache")
fi
for network_variable in \
  ALL_PROXY \
  HTTPS_PROXY \
  HTTP_PROXY \
  NO_PROXY \
  SSL_CERT_DIR \
  SSL_CERT_FILE
do
  if [[ -v "$network_variable" ]]; then
    quality_environment+=("$network_variable=${!network_variable}")
  fi
done

printf \
  'Running the mandatory isolated quality gate for commit %s.\n' \
  "$source_sha"
validate_output_directory "$output_dir" "$current_uid"
validate_public_output_binding \
  "$output_dir" \
  "$locked_output_directory" \
  "$output_identity"
validate_private_directory_binding \
  "$quality_tmp_dir" \
  "$quality_tmp_dir_physical" \
  "$quality_tmp_metadata" \
  'Quality-gate temporary directory'
(
  cd "$quality_source"
  "${quality_environment[@]}" ./scripts/check.sh
)
validate_output_directory "$output_dir" "$current_uid"
validate_public_output_binding \
  "$output_dir" \
  "$locked_output_directory" \
  "$output_identity"
validate_private_directory_binding \
  "$quality_tmp_dir" \
  "$quality_tmp_dir_physical" \
  "$quality_tmp_metadata" \
  'Quality-gate temporary directory'
validate_advisory_database_state \
  "$quality_audit_db" \
  "$rustsec_advisory_db_revision" \
  "$rustsec_advisory_db_fetch_epoch" \
  "$rustsec_advisory_db_index_checksum" \
  "$rustsec_advisory_db_config_checksum" >/dev/null
advisory_database_current_epoch="$(date -u +%s)"
validate_advisory_database_freshness \
  "$rustsec_advisory_db_fetch_epoch" \
  "$advisory_database_current_epoch" \
  "$rustsec_advisory_database_maximum_age_seconds" \
  "$rustsec_advisory_database_maximum_future_skew_seconds" || {
  printf 'The sealed RustSec advisory database is no longer fresh enough to release.\n' \
    >&2
  exit 1
}
verify_quality_source_after_gate \
  "$quality_source" \
  "$release_stage/quality-after.index" \
  "$release_stage/quality-after.untracked"
validate_release_source_state \
  "$project_dir" \
  "$source_sha" \
  "$release_tag" \
  "$version"

# The quality tree and all of its caches are disposable. The signed build gets
# a fresh immutable source extraction and a separately generated vendor tree.
rm -rf --one-file-system -- \
  "$quality_source" \
  "$quality_target_dir" \
  "$quality_isolated_home" \
  "$quality_isolated_cargo_home" \
  "$quality_tmp_dir" \
  "$quality_vendor" \
  "$quality_npm_cache"
rm -f -- \
  "$quality_source_archive" \
  "$quality_vendor_config" \
  "$release_stage/quality-after.index" \
  "$release_stage/quality-after.index.lock" \
  "$release_stage/quality-after.untracked"
create_and_verify_source_archive \
  "$first_source_archive" \
  "$release_build_source" \
  "$release_stage/source-first.index" \
  "$release_stage/source-first.untracked"

# Vendor locked dependencies before the build, then use a clean Cargo home and
# an offline source replacement. This prevents user Cargo configuration,
# compiler wrappers and network changes from affecting the signed build.
vendor_config="$release_stage/vendor-config.toml"
rm -rf -- "$release_build_source/.cargo"
install -d -m 0700 "$release_build_source/.cargo"
(
  cd "$release_build_source"
  "${vendor_environment[@]}" \
    "$cargo_command" vendor \
    --locked \
    --versioned-dirs \
    vendor > "$vendor_config"
)
[[ -s "$vendor_config" ]] || {
  printf 'Cargo produced an empty release vendor configuration.\n' >&2
  exit 1
}
install -m 0600 \
  "$vendor_config" \
  "$release_build_source/.cargo/config.toml"

release_rustflags="--remap-path-prefix=$release_build_source=/usr/src/dufs"
release_rustflags+=$'\x1f'
release_rustflags+="--remap-path-prefix=$release_stage_physical_at_creation/build-source=/usr/src/dufs"
release_rustflags+=$'\x1f'
release_rustflags+="--remap-path-prefix=$release_target_dir=/usr/src/dufs-target"
release_rustflags+=$'\x1f'
release_rustflags+="--remap-path-prefix=$release_stage_physical_at_creation/cargo-target=/usr/src/dufs-target"
release_rustflags+=$'\x1f'
release_rustflags+="--remap-path-prefix=$release_stage=/usr/src/dufs-build"
release_rustflags+=$'\x1f'
release_rustflags+="--remap-path-prefix=$release_stage_physical_at_creation=/usr/src/dufs-build"
isolated_build_environment=(
  env -i
  "CARGO=$cargo_command"
  "CARGO_BUILD_TARGET=$host_target"
  "CARGO_ENCODED_RUSTFLAGS=$release_rustflags"
  "CARGO_HOME=$release_isolated_cargo_home"
  "CARGO_INCREMENTAL=0"
  "CARGO_NET_OFFLINE=true"
  "CARGO_TARGET_DIR=$release_target_dir"
  "DUFS_BUILD_GIT_SHA=$source_sha"
  "HOME=$release_isolated_home"
  "LANG=C"
  "LC_ALL=C"
  "PATH=$release_tool_path"
  "RUSTC=$rustc_command"
  "RUSTUP_TOOLCHAIN=$required_rust_version"
  "SOURCE_DATE_EPOCH=$release_epoch"
  "TMPDIR=$release_tmp_dir"
  "TZ=UTC"
  "ZERO_AR_DATE=1"
)
if [[ -n "$host_rustup_home" ]]; then
  isolated_build_environment+=("RUSTUP_HOME=$host_rustup_home")
fi

release_metadata="$release_stage/release-metadata.json"
release_third_party_notices="$release_stage/THIRD_PARTY_LICENSES.txt"
(
  cd "$release_build_source"
  "${isolated_build_environment[@]}" \
    "$cargo_command" metadata \
    --frozen \
    --format-version 1 \
    --all-features \
    --filter-platform "$host_target" > "$release_metadata"
)
run_node_entrypoint \
  "$node_command" \
  "$release_build_source/scripts/generate-third-party-notices.mjs" \
  "$release_metadata" \
  "$release_build_source/vendor" \
  "$release_build_source" \
  "$release_third_party_notices"

(
  cd "$release_build_source"
  "${isolated_build_environment[@]}" \
    "$cargo_command" build \
    --frozen \
    --release \
    --target "$host_target" \
    --target-dir "$release_target_dir"
  "${isolated_build_environment[@]}" \
    "$cargo_command" cyclonedx \
    --manifest-path Cargo.toml \
    --format json \
    --spec-version 1.5 \
    --override-filename dufs-release.cdx \
    --all-features \
    --target "$host_target" \
    --quiet
)

release_binary="$release_target_dir/$host_target/release/dufs"
release_sbom="$release_build_source/dufs-release.cdx.json"
[[ -x "$release_binary" && -f "$release_sbom" ]] || {
  printf 'Expected release binary or SBOM was not produced.\n' >&2
  exit 1
}
release_version="$("$release_binary" --version)"
expected_release_version="dufs $version (git $source_sha)"
[[ "$release_version" == "$expected_release_version" ]] || {
  printf 'Unexpected release binary version. Expected %s; found: %s\n' \
    "$expected_release_version" \
    "$release_version" >&2
  exit 1
}
if grep -aFq -- "$release_stage" "$release_binary"; then
  printf 'Release binary still contains its private build path.\n' >&2
  exit 1
fi
if grep -aFq -- "$release_stage_physical_at_creation" "$release_binary"; then
  printf 'Release binary still contains its physical private build path.\n' >&2
  exit 1
fi

# Extract the immutable commit a second time after all build-time code has run.
# Only this fresh tree supplies package documentation and release helpers.
create_and_verify_source_archive \
  "$second_source_archive" \
  "$release_package_source" \
  "$release_stage/source-second.index" \
  "$release_stage/source-second.untracked"
first_source_archive_checksum="$(sha256sum "$first_source_archive")"
first_source_archive_checksum="${first_source_archive_checksum%% *}"
second_source_archive_checksum="$(sha256sum "$second_source_archive")"
second_source_archive_checksum="${second_source_archive_checksum%% *}"
[[ "$first_source_archive_checksum" == \
  "$second_source_archive_checksum" ]] || {
  printf 'Source archive changed between the two isolated extractions.\n' >&2
  exit 1
}
run_node_entrypoint \
  "$node_command" \
  "$release_package_source/scripts/normalize-sbom.mjs" \
  "$release_sbom" \
  "$release_build_source" \
  "$version" \
  "$source_sha" \
  "$release_stage"

short_sha="${source_sha:0:12}"
release_name="dufs-${version}-${host_target}-${short_sha}"
release_directory_name="$release_name.release"
staged_release_directory="$release_stage/$release_directory_name"
final_release_directory="$output_dir/$release_directory_name"
staged_release_via_lock="$release_stage_via_lock/$release_directory_name"
final_release_via_lock="$locked_output_directory/$release_directory_name"
install -d -m 0755 "$staged_release_directory"

package_root="$release_stage/package/$release_name"
install -d -m 0755 "$package_root"
install_release_support_tree "$release_package_source" "$package_root"
install -m 0755 "$release_binary" "$package_root/dufs"
install -m 0644 "$release_sbom" "$package_root/dufs.cdx.json"
install -m 0644 \
  "$release_third_party_notices" \
  "$package_root/THIRD_PARTY_LICENSES.txt"
install -m 0644 \
  "$release_rust_library_notice" \
  "$package_root/RUST-STANDARD-LIBRARY-COPYRIGHT.html"
write_build_environment_manifest \
  "$package_root/BUILD-ENVIRONMENT.txt" \
  "$source_sha" \
  "$version" \
  "$release_epoch" \
  "$host_target" \
  "$rustc_version" \
  "$cargo_version" \
  "$cargo_cyclonedx_version" \
  "$cargo_audit_version" \
  "$rustsec_advisory_db_revision" \
  "$rustsec_advisory_db_fetch_epoch" \
  "$node_command" \
  "$npm_command"
verify_release_documentation_layout "$package_root" "$node_command"
write_release_package_checksums "$package_root"
verify_release_package_checksum_coverage "$package_root"

archive_name="$release_name.tar.gz"
checksum_name="$archive_name.sha256"
signature_name="$checksum_name.sig"
public_key_name="$checksum_name.pub.pem"
staged_archive="$staged_release_directory/$archive_name"
staged_checksum="$staged_release_directory/$checksum_name"
staged_signature="$staged_release_directory/$signature_name"
staged_public_key="$staged_release_directory/$public_key_name"
write_reproducible_release_archive \
  "$release_stage/package" \
  "$release_name" \
  "$release_epoch" \
  "$staged_archive"
(
  cd "$staged_release_directory"
  sha256sum "$archive_name" > "$checksum_name"
  sha256sum --check "$checksum_name"
)
chmod 0644 "$staged_checksum"
validate_release_source_state \
  "$project_dir" \
  "$source_sha" \
  "$release_tag" \
  "$version"
signature_mode_file="$release_stage/signature-mode"
sign_checksum_with_validated_key \
  "$signing_key_argument" \
  "$current_uid" \
  "$staged_checksum" \
  "$staged_signature" \
  "$staged_public_key" \
  "$signature_mode_file"
signature_mode="$(<"$signature_mode_file")"
rm -f -- "$signature_mode_file"
chmod 0644 "$staged_signature" "$staged_public_key"

# Persist every complete artifact and the staged directory before exposing one
# final directory entry. No release file is individually visible beforehand.
sync -- \
  "$staged_archive" \
  "$staged_checksum" \
  "$staged_signature" \
  "$staged_public_key"
sync -- "$staged_release_directory"

validate_release_source_state \
  "$project_dir" \
  "$source_sha" \
  "$release_tag" \
  "$version"
validate_output_directory "$output_dir" "$current_uid"
[[ "$(stat -Lc '%d:%i' -- "$output_dir")" == "$output_identity" ]] || {
  printf 'Release output directory changed before publication.\n' >&2
  exit 1
}
# Both rename operands and the durability sync resolve below the already
# validated and locked directory fd. Renaming any ancestor of the public
# string path therefore cannot redirect the commit or its stage cleanup.
publish_release_directory_durably \
  "$staged_release_via_lock" \
  "$final_release_via_lock" \
  "$locked_output_directory"
validate_public_output_binding \
  "$output_dir" \
  "$locked_output_directory" \
  "$output_identity"
[[ "$(stat -Lc '%d:%i' -- "$final_release_directory")" == \
  "$(stat -Lc '%d:%i' -- "$final_release_via_lock")" ]] || {
  printf 'Published release path does not refer to the committed directory.\n' >&2
  exit 1
}
validate_public_output_binding \
  "$output_dir" \
  "$locked_output_directory" \
  "$output_identity"

printf 'Created release directory:\n'
printf '  %s\n' "$final_release_directory"
printf 'Artifacts:\n'
printf '  %s\n' \
  "$archive_name" \
  "$checksum_name" \
  "$signature_name" \
  "$public_key_name"
printf 'Source commit: %s\n' "$source_sha"
printf 'Release tag: %s\n' "$release_tag"
printf 'Signature mode: %s\n' "$signature_mode"
