#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: mobile/scripts/android-h3-smoke.sh [--device <name>] [--package <id>] [--activity <name>] [--skip-start]

Launches the Android app, auto-starts managed VPN, opens Cloudflare trace in Chrome,
reloads once, and extracts the reported HTTP protocol from the page.

Success criteria:
  - First load eventually returns an `http=` line
  - One of the follow-up requests upgrades to `http=http/3`

Examples:
  mobile/scripts/android-h3-smoke.sh
  mobile/scripts/android-h3-smoke.sh --device PHP110
  mobile/scripts/android-h3-smoke.sh --device PHP110 --skip-start
EOF
}

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)

DEVICE_NAME=""
PACKAGE_NAME="io.hysteria.mobile"
MAIN_ACTIVITY="dev.dioxus.main.MainActivity"
SKIP_START=false
FOLLOW_UP_ATTEMPTS=3
SAMPLE_WAIT_SECONDS=8
EXTRACT_RETRIES=3

resolve_adb_target() {
  if [[ -z "$DEVICE_NAME" ]]; then
    return 0
  fi
  python3 - "$DEVICE_NAME" <<'PY'
import subprocess, sys
needle = sys.argv[1].lower()
matches = []
for line in subprocess.check_output(["adb", "devices", "-l"], text=True).splitlines():
    if not line or line.startswith("List of devices attached"):
        continue
    if " device " not in line:
        continue
    serial = line.split()[0]
    if needle in line.lower():
        matches.append(serial)
if not matches:
    raise SystemExit(f"could not resolve adb device for pattern: {needle}")
if len(matches) > 1:
    raise SystemExit(f"multiple adb devices matched {needle}: {matches}")
print(matches[0])
PY
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --device)
      DEVICE_NAME="${2:-}"
      if [[ -z "$DEVICE_NAME" ]]; then
        echo "--device requires a value" >&2
        exit 1
      fi
      shift 2
      ;;
    --package)
      PACKAGE_NAME="${2:-}"
      if [[ -z "$PACKAGE_NAME" ]]; then
        echo "--package requires a value" >&2
        exit 1
      fi
      shift 2
      ;;
    --activity)
      MAIN_ACTIVITY="${2:-}"
      if [[ -z "$MAIN_ACTIVITY" ]]; then
        echo "--activity requires a value" >&2
        exit 1
      fi
      shift 2
      ;;
    --skip-start)
      SKIP_START=true
      shift
      ;;
    --followup-attempts)
      FOLLOW_UP_ATTEMPTS="${2:-}"
      if [[ -z "$FOLLOW_UP_ATTEMPTS" ]]; then
        echo "--followup-attempts requires a value" >&2
        exit 1
      fi
      shift 2
      ;;
    --sample-wait)
      SAMPLE_WAIT_SECONDS="${2:-}"
      if [[ -z "$SAMPLE_WAIT_SECONDS" ]]; then
        echo "--sample-wait requires a value" >&2
        exit 1
      fi
      shift 2
      ;;
    --extract-retries)
      EXTRACT_RETRIES="${2:-}"
      if [[ -z "$EXTRACT_RETRIES" ]]; then
        echo "--extract-retries requires a value" >&2
        exit 1
      fi
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

export PATH="/home/de/android-sdk/platform-tools:${PATH}"

ADB_SERIAL="$(resolve_adb_target)"
ADB_ARGS=()
if [[ -n "${ADB_SERIAL:-}" ]]; then
  ADB_ARGS+=( -s "$ADB_SERIAL" )
fi

adb_cmd() {
  adb "${ADB_ARGS[@]}" "$@"
}

extract_http_protocol() {
  local dump_name="$1"
  local dump_path="/sdcard/${dump_name}.xml"
  adb_cmd shell uiautomator dump "$dump_path" >/dev/null
  local xml
  xml=$(adb_cmd shell cat "$dump_path")
  python3 - "$xml" <<'PY'
import re
import sys
import xml.etree.ElementTree as ET

xml = sys.argv[1]
root = ET.fromstring(xml)
texts = []
for node in root.iter("node"):
    text = node.attrib.get("text", "")
    if text:
        texts.append(text)
blob = "\n".join(texts).replace("&#10;", "\n")
match = re.search(r"http=([^\s]+)", blob)
if not match:
    raise SystemExit(1)
print(match.group(1))
PY
}

open_trace_url() {
  local url="$1"
  adb_cmd shell am start -a android.intent.action.VIEW -d "$url" com.android.chrome >/dev/null
}

reload_trace_page() {
  adb_cmd shell input keyevent 61 >/dev/null
  adb_cmd shell input keyevent 66 >/dev/null
}

collect_http_protocol() {
  local dump_prefix="$1"
  local retries="$2"
  local sleep_seconds="$3"
  local attempt
  for attempt in $(seq 1 "$retries"); do
    local protocol=""
    if protocol=$(extract_http_protocol "${dump_prefix}_${attempt}" 2>/dev/null); then
      echo "$protocol"
      return 0
    fi
    if [[ "$attempt" -lt "$retries" ]]; then
      sleep "$sleep_seconds"
    fi
  done
  return 1
}

wait_for_tun0() {
  local attempts=15
  local delay=2
  for ((i=1; i<=attempts; i++)); do
    if adb_cmd shell ip addr show tun0 >/tmp/h3-smoke-tun0.$$ 2>/dev/null; then
      cat /tmp/h3-smoke-tun0.$$
      rm -f /tmp/h3-smoke-tun0.$$
      return 0
    fi
    sleep "$delay"
  done
  rm -f /tmp/h3-smoke-tun0.$$ || true
  echo "timed out waiting for tun0" >&2
  return 1
}

echo "==> Clearing logcat"
adb_cmd logcat -c

if [[ "$SKIP_START" != true ]]; then
  echo "==> Restarting $PACKAGE_NAME"
  adb_cmd shell am force-stop "$PACKAGE_NAME"
  adb_cmd shell am start -n "$PACKAGE_NAME/$MAIN_ACTIVITY" --ez io.hysteria.mobile.extra.AUTO_START_VPN true
fi

echo "==> Waiting for managed VPN"
wait_for_tun0

TRACE_URL="https://www.cloudflare.com/cdn-cgi/trace"
echo "==> Opening $TRACE_URL"
open_trace_url "$TRACE_URL"
sleep "$SAMPLE_WAIT_SECONDS"

echo "==> Reading first Cloudflare trace result"
FIRST_HTTP=""
if ! FIRST_HTTP=$(collect_http_protocol "cf_trace_first" "$EXTRACT_RETRIES" 2); then
  echo "first trace result not ready; re-opening once" >&2
  open_trace_url "$TRACE_URL"
  sleep "$SAMPLE_WAIT_SECONDS"
  if ! FIRST_HTTP=$(collect_http_protocol "cf_trace_first_retry" "$EXTRACT_RETRIES" 2); then
    echo "failed to read first Cloudflare trace result after reopen" >&2
    exit 1
  fi
fi
echo "First trace protocol: $FIRST_HTTP"

FOLLOW_UP_HTTP=""
for attempt in $(seq 1 "$FOLLOW_UP_ATTEMPTS"); do
  if [[ "$attempt" -eq 1 ]]; then
    echo "==> Reloading page"
    reload_trace_page
  else
    echo "==> Re-opening trace page (attempt $attempt/$FOLLOW_UP_ATTEMPTS)"
    open_trace_url "$TRACE_URL"
  fi
  sleep "$SAMPLE_WAIT_SECONDS"

  if ! FOLLOW_UP_HTTP=$(collect_http_protocol "cf_trace_followup_${attempt}" "$EXTRACT_RETRIES" 2); then
    echo "Follow-up protocol ($attempt/$FOLLOW_UP_ATTEMPTS): <unavailable>" >&2
    continue
  fi
  echo "Follow-up protocol ($attempt/$FOLLOW_UP_ATTEMPTS): $FOLLOW_UP_HTTP"
  if [[ "$FOLLOW_UP_HTTP" == "http/3" ]]; then
    echo "H3 smoke passed"
    exit 0
  fi
done

echo "H3 smoke failed: expected one follow-up trace request to upgrade to http/3" >&2
exit 1
