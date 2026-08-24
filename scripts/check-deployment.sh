#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"

deployment_mode="normal"
if [[ "$#" -gt 1 ]]; then
  printf 'Usage: %s [--self-test]\n' "${0##*/}" >&2
  exit 2
elif [[ "$#" -eq 1 ]]; then
  case "$1" in
    --self-test)
      deployment_mode="self-test"
      ;;
    --self-test-fail-after-validation-dir)
      deployment_mode="fail-after-validation-dir"
      ;;
    *)
      printf 'Usage: %s [--self-test]\n' "${0##*/}" >&2
      exit 2
      ;;
  esac
fi

for command_name in \
  cargo \
  chmod \
  curl \
  grep \
  head \
  install \
  mktemp \
  nginx \
  node \
  rm \
  sed \
  systemd-analyze
do
  command -v "$command_name" >/dev/null 2>&1 || {
    printf 'Required deployment validator is unavailable: %s\n' "$command_name" >&2
    exit 1
  }
done

run_early_cleanup_self_test() {
  (
    local self_test_tmp_root self_test_dir child_status
    local -a leftovers

    self_test_tmp_root="${TMPDIR:-/tmp}"
    self_test_tmp_root="$(cd -P -- "$self_test_tmp_root" && pwd -P)"
    self_test_dir=""

    cleanup_self_test() {
      local status=$?

      trap - EXIT HUP INT TERM
      set +e
      if [[ -n "$self_test_dir" && \
        "${self_test_dir%/*}" == "$self_test_tmp_root" && \
        "${self_test_dir##*/}" == dufs-deployment-trap-test.* ]]
      then
        rm -rf --one-file-system -- "$self_test_dir"
      fi
      exit "$status"
    }

    trap cleanup_self_test EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM

    self_test_dir="$(
      mktemp -d -p "$self_test_tmp_root" \
        dufs-deployment-trap-test.XXXXXXXX
    )"
    chmod 0700 "$self_test_dir"

    set +e
    TMPDIR="$self_test_dir" \
      "$BASH" "$project_dir/scripts/check-deployment.sh" \
      --self-test-fail-after-validation-dir >/dev/null 2>&1
    child_status=$?
    set -e
    if [[ "$child_status" -ne 97 ]]; then
      printf 'Early-cleanup self-test returned %s instead of 97.\n' \
        "$child_status" >&2
      exit 1
    fi

    shopt -s nullglob
    leftovers=(
      "$self_test_dir"/dufs-deployment.*
      "$self_test_dir"/dufs-deployment-sockets.*
    )
    if [[ "${#leftovers[@]}" -ne 0 ]]; then
      printf 'Early-cleanup self-test left temporary resources behind:\n' >&2
      printf '  %s\n' "${leftovers[@]}" >&2
      exit 1
    fi

    printf 'Early cleanup trap self-test passed.\n'
  )
}

if [[ "$deployment_mode" == "self-test" ]]; then
  run_early_cleanup_self_test
  exit 0
elif [[ "$deployment_mode" == "normal" ]]; then
  run_early_cleanup_self_test
fi

tmp_root="${TMPDIR:-/tmp}"
tmp_root="$(cd -P -- "$tmp_root" && pwd -P)"
validation_dir=""
socket_dir=""
upstream_pid=""
nginx_pid=""

stop_nginx() {
  if [[ -n "$nginx_pid" ]]; then
    kill -TERM "$nginx_pid" 2>/dev/null || true
    wait "$nginx_pid" 2>/dev/null || true
    nginx_pid=""
  fi
}

cleanup() {
  local status=$?

  trap - EXIT HUP INT TERM
  set +e
  stop_nginx
  if [[ -n "$upstream_pid" ]]; then
    kill -TERM "$upstream_pid" 2>/dev/null
    wait "$upstream_pid" 2>/dev/null
  fi
  if [[ -n "$validation_dir" ]]; then
    if [[ "${validation_dir%/*}" == "$tmp_root" && \
      "${validation_dir##*/}" == dufs-deployment.* ]]
    then
      rm -rf --one-file-system -- "$validation_dir"
    else
      printf 'Refusing to remove unexpected deployment path: %s\n' \
        "$validation_dir" >&2
      status=1
    fi
  fi
  if [[ -n "$socket_dir" ]]; then
    if [[ "${socket_dir%/*}" == "$tmp_root" && \
      "${socket_dir##*/}" == dufs-deployment-sockets.* ]]
    then
      rm -rf --one-file-system -- "$socket_dir"
    else
      printf 'Refusing to remove unexpected deployment socket path: %s\n' \
        "$socket_dir" >&2
      status=1
    fi
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

validation_dir="$(mktemp -d -p "$tmp_root" dufs-deployment.XXXXXXXX)"
chmod 0700 "$validation_dir"
[[ "${validation_dir%/*}" == "$tmp_root" && \
  "${validation_dir##*/}" == dufs-deployment.* ]] || {
  printf 'Deployment validator created an unexpected temporary path.\n' >&2
  exit 1
}
if [[ "$deployment_mode" == "fail-after-validation-dir" ]]; then
  printf 'Injected failure after validation directory creation.\n' >&2
  exit 97
fi
# Nginx may drop privileges for worker processes. Keep configs, keys and logs
# in the private validation directory while exposing only transient Unix
# sockets through a separate searchable directory.
socket_dir="$(mktemp -d -p "$tmp_root" dufs-deployment-sockets.XXXXXXXX)"
chmod 0755 "$socket_dir"
[[ "${socket_dir%/*}" == "$tmp_root" && \
  "${socket_dir##*/}" == dufs-deployment-sockets.* ]] || {
  printf 'Deployment validator created an unexpected socket path.\n' >&2
  exit 1
}
special_checkout="$validation_dir/checkout with spaces & # \\ path"
runtime_dir="$validation_dir/runtime"
install -d -m 0755 \
  "$special_checkout/deploy" \
  "$special_checkout/tests/data" \
  "$special_checkout/tests/deployment" \
  "$runtime_dir"
install -m 0644 \
  "$project_dir/deploy/dufs.service" \
  "$project_dir/deploy/nginx-dufs.conf" \
  "$project_dir/deploy/dufs-proxy.conf" \
  "$special_checkout/deploy/"
install -m 0644 \
  "$project_dir/tests/data/cert.pem" \
  "$project_dir/tests/data/key_pkcs8.pem" \
  "$special_checkout/tests/data/"
install -m 0644 \
  "$project_dir/tests/deployment/mock-upstream.mjs" \
  "$special_checkout/tests/deployment/"
install -m 0644 \
  "$special_checkout/tests/data/cert.pem" \
  "$runtime_dir/cert.pem"
install -m 0600 \
  "$special_checkout/tests/data/key_pkcs8.pem" \
  "$runtime_dir/key.pem"
install -m 0644 \
  "$special_checkout/deploy/dufs-proxy.conf" \
  "$runtime_dir/dufs-proxy.conf"
install -m 0644 \
  "$special_checkout/tests/deployment/mock-upstream.mjs" \
  "$runtime_dir/mock-upstream.mjs"

sed \
  's#/opt/dufs/bin/dufs#/bin/true#' \
  "$special_checkout/deploy/dufs.service" \
  > "$validation_dir/dufs.service"
systemd-analyze verify "$validation_dir/dufs.service"

sed \
  -e "s#/etc/dufs/tls/fullchain.pem#$runtime_dir/cert.pem#" \
  -e "s#/etc/dufs/tls/private.key#$runtime_dir/key.pem#" \
  -e "s#/etc/nginx/snippets/dufs-proxy.conf#$runtime_dir/dufs-proxy.conf#g" \
  "$special_checkout/deploy/nginx-dufs.conf" \
  > "$validation_dir/dufs.conf"
if grep -Fq -- "$special_checkout" "$validation_dir/dufs.conf"; then
  printf 'Generated Nginx config retained an unsafe checkout path.\n' >&2
  exit 1
fi

# `nginx -t` opens configured listeners. Rewrite every production endpoint to
# a private Unix socket before validation so this gate exercises the complete
# config as the same unprivileged user used by GitHub-hosted runners.
sed \
  -e "s#server 127\\.0\\.0\\.1:5000;#server unix:$socket_dir/upstream.sock;#" \
  -e "s#listen 80 default_server;#listen unix:$socket_dir/http.sock default_server;#" \
  -e "s#listen \[::\]:80 default_server;#listen unix:$socket_dir/http-v6.sock default_server;#" \
  -e "s#listen 443 ssl http2 default_server;#listen unix:$socket_dir/https.sock ssl http2 default_server;#" \
  -e "s#listen \[::\]:443 ssl http2 default_server;#listen unix:$socket_dir/https-v6.sock ssl http2 default_server;#" \
  -e "s#listen 80;#listen unix:$socket_dir/http.sock;#" \
  -e "s#listen \[::\]:80;#listen unix:$socket_dir/http-v6.sock;#" \
  -e "s#listen 443 ssl http2;#listen unix:$socket_dir/https.sock ssl http2;#" \
  -e "s#listen \[::\]:443 ssl http2;#listen unix:$socket_dir/https-v6.sock ssl http2;#" \
  "$validation_dir/dufs.conf" \
  > "$validation_dir/active-dufs.conf"

assert_active_occurrences() {
  local expected="$1"
  local needle="$2"
  local observed

  observed="$(grep -Fc -- "$needle" "$validation_dir/active-dufs.conf" || true)"
  if [[ "$observed" -ne "$expected" ]]; then
    printf 'Active Nginx config expected %s occurrence(s), found %s: %s\n' \
      "$expected" "$observed" "$needle" >&2
    exit 1
  fi
}
assert_active_occurrences 1 "server unix:$socket_dir/upstream.sock;"
assert_active_occurrences 2 "listen unix:$socket_dir/http.sock"
assert_active_occurrences 2 "listen unix:$socket_dir/http-v6.sock"
assert_active_occurrences 2 "listen unix:$socket_dir/https.sock"
assert_active_occurrences 2 "listen unix:$socket_dir/https-v6.sock"
while IFS= read -r active_listener; do
  if [[ ! "$active_listener" =~ ^[[:space:]]*listen[[:space:]]+unix: ]]; then
    printf 'Active Nginx config retained a network listener: %s\n' \
      "$active_listener" >&2
    exit 1
  fi
done < <(
  grep -E '^[[:space:]]*listen[[:space:]]+' \
    "$validation_dir/active-dufs.conf"
)
if grep -Eq \
  '^[[:space:]]*server[[:space:]]+127\.0\.0\.1:5000;' \
  "$validation_dir/active-dufs.conf"
then
  printf 'Active Nginx config retained a production network endpoint.\n' >&2
  exit 1
fi
{
  printf 'pid "%s/nginx-active.pid";\n' "$validation_dir"
  printf 'error_log "%s/nginx-active-error.log" notice;\n' "$validation_dir"
  printf 'events {}\n'
  printf 'http {\n'
  printf '  access_log off;\n'
  printf '  include "%s/active-dufs.conf";\n' "$validation_dir"
  printf '}\n'
} > "$validation_dir/nginx-active.conf"
nginx -t -p "$validation_dir/" -c "$validation_dir/nginx-active.conf"

node \
  "$runtime_dir/mock-upstream.mjs" \
  "$socket_dir/upstream.sock" \
  > "$validation_dir/upstream.log" \
  2>&1 &
upstream_pid=$!
for _ in {1..100}; do
  [[ -S "$socket_dir/upstream.sock" ]] && break
  kill -0 "$upstream_pid" 2>/dev/null || {
    printf 'Deployment test upstream exited during startup.\n' >&2
    sed -n '1,120p' "$validation_dir/upstream.log" >&2
    exit 1
  }
  sleep 0.05
done
[[ -S "$socket_dir/upstream.sock" ]] || {
  printf 'Deployment test upstream did not create its socket.\n' >&2
  exit 1
}
chmod 0777 "$socket_dir/upstream.sock"

start_nginx() {
  rm -f -- \
    "$socket_dir/http.sock" \
    "$socket_dir/http-v6.sock" \
    "$socket_dir/https.sock" \
    "$socket_dir/https-v6.sock" \
    "$validation_dir/nginx-active.pid"
  nginx \
    -p "$validation_dir/" \
    -c "$validation_dir/nginx-active.conf" \
    -g 'daemon off;' \
    > "$validation_dir/nginx-active.log" \
    2>&1 &
  nginx_pid=$!
  for _ in {1..100}; do
    if [[ -S "$socket_dir/http.sock" && \
      -S "$socket_dir/http-v6.sock" && \
      -S "$socket_dir/https.sock" && \
      -S "$socket_dir/https-v6.sock" ]]
    then
      return 0
    fi
    kill -0 "$nginx_pid" 2>/dev/null || {
      printf 'Nginx exited during active deployment-test startup.\n' >&2
      sed -n '1,160p' "$validation_dir/nginx-active.log" >&2
      sed -n '1,160p' "$validation_dir/nginx-active-error.log" >&2
      return 1
    }
    sleep 0.05
  done
  printf 'Nginx did not create its deployment-test sockets.\n' >&2
  return 1
}

start_nginx
curl \
  --noproxy '*' \
  --silent \
  --show-error \
  --unix-socket "$socket_dir/http.sock" \
  --dump-header "$validation_dir/redirect.headers" \
  --output /dev/null \
  'http://files.example.com/folder?value=1'
grep -Eq '^HTTP/[0-9.]+ 308' "$validation_dir/redirect.headers"
grep -Eiq \
  '^location: https://files\.example\.com/folder\?value=1[[:space:]]*$' \
  "$validation_dir/redirect.headers"
if curl \
  --max-time 3 \
  --noproxy '*' \
  --silent \
  --show-error \
  --unix-socket "$socket_dir/http.sock" \
  --output /dev/null \
  'http://unknown.example.invalid/' 2>/dev/null
then
  printf 'Unknown HTTP Host was not rejected by the default server.\n' >&2
  exit 1
fi
if curl \
  --insecure \
  --max-time 3 \
  --noproxy '*' \
  --silent \
  --show-error \
  --unix-socket "$socket_dir/https.sock" \
  --output /dev/null \
  'https://unknown.example.invalid/' 2>/dev/null
then
  printf 'Unknown HTTPS SNI was not rejected by the default server.\n' >&2
  exit 1
fi
if curl \
  --header 'Host: unknown.example.invalid' \
  --insecure \
  --max-time 3 \
  --noproxy '*' \
  --silent \
  --show-error \
  --unix-socket "$socket_dir/https.sock" \
  --output /dev/null \
  'https://files.example.com/' 2>/dev/null
then
  printf 'Unknown HTTPS Host was accepted with valid SNI.\n' >&2
  exit 1
fi
curl \
  --fail \
  --header 'X-Forwarded-For: 203.0.113.99' \
  --http1.1 \
  --insecure \
  --noproxy '*' \
  --silent \
  --show-error \
  --unix-socket "$socket_dir/https.sock" \
  --dump-header "$validation_dir/upstream.headers" \
  --output "$validation_dir/upstream.json" \
  'https://files.example.com/echo?value=1'
grep -Eiq \
  '^strict-transport-security: max-age=31536000[[:space:]]*$' \
  "$validation_dir/upstream.headers"
node -e '
  const response = JSON.parse(require("fs").readFileSync(process.argv[1]));
  if (
    response.host !== "files.example.com" ||
    response.x_forwarded_host !== "files.example.com" ||
    response.x_forwarded_for !== "unix:" ||
    response.x_forwarded_proto !== "https" ||
    response.http_version !== "1.1" ||
    response.connection !== "" ||
    response.url !== "/echo?value=1"
  ) {
    throw new Error(`unexpected upstream request: ${JSON.stringify(response)}`);
  }
' "$validation_dir/upstream.json"

head -c 5000 /dev/zero > "$validation_dir/large-login-body"
for login_path in \
  '/__dufs__/login' \
  '/__dufs__/login/' \
  '/__dufs__/%6cogin' \
  '/__dufs__//login'
do
  login_status="$(
    curl \
      --http1.1 \
      --insecure \
      --noproxy '*' \
      --path-as-is \
      --silent \
      --show-error \
      --unix-socket "$socket_dir/https.sock" \
      --request POST \
      --data-binary "@$validation_dir/large-login-body" \
      --output /dev/null \
      --write-out '%{http_code}' \
      "https://files.example.com${login_path}"
  )"
  [[ "$login_status" == "413" ]] || {
    printf 'Login route variant escaped the 4 KiB body limit: %s (%s)\n' \
      "$login_path" \
      "$login_status" >&2
    exit 1
  }
done
ordinary_status="$(
  curl \
    --http1.1 \
    --insecure \
    --noproxy '*' \
    --silent \
    --show-error \
    --unix-socket "$socket_dir/https.sock" \
    --request POST \
    --data-binary "@$validation_dir/large-login-body" \
    --output /dev/null \
    --write-out '%{http_code}' \
    'https://files.example.com/ordinary'
)"
[[ "$ordinary_status" == "200" ]]
stop_nginx

start_nginx
connection_pids=()
for index in {1..5}; do
  curl \
    --http1.1 \
    --insecure \
    --noproxy '*' \
    --silent \
    --show-error \
    --unix-socket "$socket_dir/https.sock" \
    --request POST \
    --output /dev/null \
    --write-out '%{http_code}\n' \
    "https://files.example.com/__dufs__/login/hold?request=${index}" \
    > "$validation_dir/connection-${index}.status" &
  connection_pids+=("$!")
done
for connection_pid in "${connection_pids[@]}"; do
  wait "$connection_pid"
done
grep -qx '429' "$validation_dir"/connection-*.status || {
  printf 'Nginx login connection limit did not reject excess concurrency.\n' >&2
  exit 1
}
grep -qx '200' "$validation_dir"/connection-*.status || {
  printf 'Nginx login connection test admitted no request.\n' >&2
  exit 1
}
sleep 13
connection_recovery_status="$(
  curl \
    --http1.1 \
    --insecure \
    --noproxy '*' \
    --silent \
    --show-error \
    --unix-socket "$socket_dir/https.sock" \
    --request POST \
    --output /dev/null \
    --write-out '%{http_code}' \
    'https://files.example.com/__dufs__/login?connection-recovery=1'
)"
[[ "$connection_recovery_status" == "200" ]] || {
  printf 'Nginx login limits did not recover after connections closed: %s\n' \
    "$connection_recovery_status" >&2
  exit 1
}
stop_nginx

start_nginx
for index in {1..7}; do
  curl \
    --http1.1 \
    --insecure \
    --noproxy '*' \
    --silent \
    --show-error \
    --unix-socket "$socket_dir/https.sock" \
    --request POST \
    --output /dev/null \
    --write-out '%{http_code}\n' \
    "https://files.example.com/__dufs__/login?request=${index}" \
    > "$validation_dir/rate-${index}.status"
done
grep -qx '429' "$validation_dir"/rate-*.status || {
  printf 'Nginx login rate limit did not reject a burst.\n' >&2
  exit 1
}
grep -qx '200' "$validation_dir"/rate-*.status || {
  printf 'Nginx login rate test admitted no request.\n' >&2
  exit 1
}
sleep 13
rate_recovery_status="$(
  curl \
    --http1.1 \
    --insecure \
    --noproxy '*' \
    --silent \
    --show-error \
    --unix-socket "$socket_dir/https.sock" \
    --request POST \
    --output /dev/null \
    --write-out '%{http_code}' \
    'https://files.example.com/__dufs__/login?rate-recovery=1'
)"
[[ "$rate_recovery_status" == "200" ]] || {
  printf 'Nginx login token bucket did not recover after a 429: %s\n' \
    "$rate_recovery_status" >&2
  exit 1
}
stop_nginx

cargo test --locked --test config deployment_yaml_example_parses -- --exact

printf 'systemd, active nginx boundary, and Dufs YAML examples are valid\n'
