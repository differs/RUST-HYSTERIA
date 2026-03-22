#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: mobile/scripts/android-push-ca.sh [--device <name>] [--package <id>] [--name <target>] [--list] <cert> [<cert>...]

Push one or more CA files into the Android app-specific CA directory so the
mobile client can pick them from the in-app selector.

Examples:
  mobile/scripts/android-push-ca.sh core/internal/integration_tests/test.crt
  mobile/scripts/android-push-ca.sh --device PHP110 --name custom_ca.crt app/internal/http/test.crt
  mobile/scripts/android-push-ca.sh --device PHP110 --list
EOF
}

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)

DEVICE_NAME=""
PACKAGE_NAME="io.hysteria.mobile"
TARGET_NAME=""
LIST_ONLY=false
FILES=()

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
    --name)
      TARGET_NAME="${2:-}"
      if [[ -z "$TARGET_NAME" ]]; then
        echo "--name requires a value" >&2
        exit 1
      fi
      shift 2
      ;;
    --list)
      LIST_ONLY=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      FILES+=("$1")
      shift
      ;;
  esac
done

if [[ "$LIST_ONLY" != true && ${#FILES[@]} -eq 0 ]]; then
  usage >&2
  exit 1
fi

if [[ -n "$TARGET_NAME" && ${#FILES[@]} -ne 1 ]]; then
  echo "--name can only be used when pushing a single file" >&2
  exit 1
fi

ADB_SERIAL="$(resolve_adb_target)"
ADB_ARGS=()
if [[ -n "${ADB_SERIAL:-}" ]]; then
  ADB_ARGS+=( -s "$ADB_SERIAL" )
fi

TARGET_DIR="/sdcard/Android/data/$PACKAGE_NAME/files/certs"

echo "==> Ensuring CA directory exists: $TARGET_DIR"
adb "${ADB_ARGS[@]}" shell "mkdir -p '$TARGET_DIR'"

if [[ "$LIST_ONLY" == true ]]; then
  echo "==> Listing CA directory"
  adb "${ADB_ARGS[@]}" shell "ls -l '$TARGET_DIR'"
  exit 0
fi

for src in "${FILES[@]}"; do
  if [[ ! -f "$src" ]]; then
    echo "CA file not found: $src" >&2
    exit 1
  fi

  target_basename="${TARGET_NAME:-$(basename -- "$src")}"
  target_path="$TARGET_DIR/$target_basename"
  echo "==> Pushing $src -> $target_path"
  adb "${ADB_ARGS[@]}" push "$src" "$target_path"
  echo "Installed CA: $target_path"
  TARGET_NAME=""
done

echo "==> Final CA directory contents"
adb "${ADB_ARGS[@]}" shell "ls -l '$TARGET_DIR'"
