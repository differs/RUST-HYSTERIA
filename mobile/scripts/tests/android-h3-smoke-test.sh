#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../../.." && pwd)
TARGET_SCRIPT="$REPO_ROOT/mobile/scripts/android-h3-smoke.sh"

assert_contains() {
  local file="$1"
  local needle="$2"
  if ! grep -Fq "$needle" "$file"; then
    echo "expected to find '$needle' in $file" >&2
    echo "--- $file ---" >&2
    cat "$file" >&2
    exit 1
  fi
}

run_smoke_with_fake_adb() {
  local scenario="$1"
  local stdout_file="$2"
  local stderr_file="$3"
  local state_dir
  state_dir=$(mktemp -d)

  adb() {
    local state_dir="${FAKE_ADB_STATE_DIR:?}"
    local scenario="${FAKE_ADB_SCENARIO:?}"
    if [[ "${1:-}" == "logcat" && "${2:-}" == "-c" ]]; then
      return 0
    fi
    if [[ "${1:-}" == "shell" && "${2:-}" == "ip" && "${3:-}" == "addr" && "${4:-}" == "show" && "${5:-}" == "tun0" ]]; then
      printf '42: tun0: <POINTOPOINT,UP,LOWER_UP> mtu 1500\n'
      printf '    inet 10.8.0.2/32 scope global tun0\n'
      return 0
    fi
    if [[ "${1:-}" == "shell" && "${2:-}" == "am" ]]; then
      return 0
    fi
    if [[ "${1:-}" == "shell" && "${2:-}" == "input" ]]; then
      return 0
    fi
    if [[ "${1:-}" == "shell" && "${2:-}" == "uiautomator" && "${3:-}" == "dump" ]]; then
      printf 'UI hierchary dumped to: %s\n' "${4:-}"
      return 0
    fi
    if [[ "${1:-}" == "shell" && "${2:-}" == "cat" ]]; then
      local path="${3:-}"
      local base
      base=$(basename "$path" .xml)
      case "$scenario:$base" in
        first-read-fails:cf_trace_first_1|first-read-fails:cf_trace_first_retry_1|followup-upgrades-to-h3:cf_trace_followup_1_1)
          cat <<'EOF'
<hierarchy rotation="0">
  <node text="ERR_CONNECTION_RESET"/>
</hierarchy>
EOF
          return 0
          ;;
        followup-upgrades-to-h3:cf_trace_first_1)
          cat <<'EOF'
<hierarchy rotation="0">
  <node text="fl=29f77"/>
  <node text="http=http/2"/>
</hierarchy>
EOF
          return 0
          ;;
        followup-upgrades-to-h3:cf_trace_followup_2_1)
          cat <<'EOF'
<hierarchy rotation="0">
  <node text="fl=29f77"/>
  <node text="http=http/3"/>
</hierarchy>
EOF
          return 0
          ;;
      esac
      echo "unexpected fake adb cat path: $path" >&2
      return 1
    fi
    echo "unexpected fake adb invocation: $*" >&2
    return 1
  }

  sleep() {
    return 0
  }

  export -f adb
  export -f sleep
  export FAKE_ADB_STATE_DIR="$state_dir"
  export FAKE_ADB_SCENARIO="$scenario"

  set +e
  bash "$TARGET_SCRIPT" --skip-start --extract-retries 1 --followup-attempts 2 --sample-wait 0 >"$stdout_file" 2>"$stderr_file"
  local exit_code=$?
  set -e

  rm -rf "$state_dir"
  return "$exit_code"
}

main() {
  local temp_dir
  temp_dir=$(mktemp -d)
  trap "rm -rf '$temp_dir'" EXIT

  local stdout_file="$temp_dir/first-read.stdout"
  local stderr_file="$temp_dir/first-read.stderr"
  if run_smoke_with_fake_adb "first-read-fails" "$stdout_file" "$stderr_file"; then
    echo "expected first-read-fails scenario to exit non-zero" >&2
    exit 1
  fi
  assert_contains "$stderr_file" "failed to read first Cloudflare trace result after reopen"

  stdout_file="$temp_dir/followup.stdout"
  stderr_file="$temp_dir/followup.stderr"
  run_smoke_with_fake_adb "followup-upgrades-to-h3" "$stdout_file" "$stderr_file"
  assert_contains "$stdout_file" "First trace protocol: http/2"
  assert_contains "$stdout_file" "Follow-up protocol (2/2): http/3"
  assert_contains "$stdout_file" "H3 smoke passed"

  echo "android-h3-smoke tests passed"
}

main "$@"
