#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: mobile/scripts/android-debug-build.sh [--release] [--device <name>] [--install] [--launch] [--skip-dx]

Build the Android app with a repo-local workaround for the current Dioxus 0.7
namespace/BuildConfig mismatch on Android.

Examples:
  mobile/scripts/android-debug-build.sh
  mobile/scripts/android-debug-build.sh --install --launch
  mobile/scripts/android-debug-build.sh --device PHP110 --install --launch
EOF
}

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
MOBILE_DIR="$REPO_ROOT/mobile"

BUILD_PROFILE=debug
INSTALL_APK=false
LAUNCH_APP=false
SKIP_DX=false
DEVICE_NAME=""

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
    --release)
      BUILD_PROFILE=release
      shift
      ;;
    --install)
      INSTALL_APK=true
      shift
      ;;
    --launch)
      LAUNCH_APP=true
      shift
      ;;
    --skip-dx)
      SKIP_DX=true
      shift
      ;;
    --device)
      DEVICE_NAME="${2:-}"
      if [[ -z "$DEVICE_NAME" ]]; then
        echo "--device requires a value" >&2
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

export ANDROID_SDK_ROOT="${ANDROID_SDK_ROOT:-/usr/lib/android-sdk}"
export ANDROID_HOME="${ANDROID_HOME:-$ANDROID_SDK_ROOT}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_SDK_ROOT/ndk/28.2.13676358}"
export JAVA_HOME="${JAVA_HOME:-/usr/lib/jvm/java-21-openjdk-amd64}"
export PATH="$ANDROID_HOME/platform-tools:$ANDROID_HOME/cmdline-tools/latest/bin:$HOME/.cargo/bin${PATH:+:$PATH}"

DX_ARGS=(build --android --verbose)
if [[ "$BUILD_PROFILE" == "release" ]]; then
  DX_ARGS+=(--release)
fi
if [[ -n "$DEVICE_NAME" ]]; then
  DX_ARGS+=(--device "$DEVICE_NAME")
fi

if [[ "$SKIP_DX" != true ]]; then
  echo "==> Running dx ${DX_ARGS[*]}"
  set +e
  (
    cd "$MOBILE_DIR"
    dx "${DX_ARGS[@]}"
  )
  DX_STATUS=$?
  set -e
  if [[ $DX_STATUS -ne 0 ]]; then
    echo "dx build exited with status $DX_STATUS; continuing with Android shim workaround"
  fi
fi

ANDROID_APP_ROOT="$REPO_ROOT/target/dx/hysteria-mobile/$BUILD_PROFILE/android/app"
APP_MODULE_ROOT="$ANDROID_APP_ROOT/app"
MODULE_GRADLE="$APP_MODULE_ROOT/build.gradle.kts"
MANIFEST_PATH="$APP_MODULE_ROOT/src/main/AndroidManifest.xml"

if [[ ! -f "$MODULE_GRADLE" ]]; then
  echo "Expected generated Gradle module at $MODULE_GRADLE" >&2
  exit 1
fi

APPLICATION_ID=$(
  python3 - "$MODULE_GRADLE" <<'PY'
import re, sys, pathlib
text = pathlib.Path(sys.argv[1]).read_text()
m = re.search(r'applicationId\s*=\s*"([^"]+)"', text)
if not m:
    raise SystemExit("could not parse applicationId")
print(m.group(1))
PY
)

MAIN_ACTIVITY=$(
  python3 - "$MANIFEST_PATH" <<'PY'
import sys, xml.etree.ElementTree as ET
root = ET.parse(sys.argv[1]).getroot()
ns = {"android": "http://schemas.android.com/apk/res/android"}
activity = root.find(".//application/activity", ns)
if activity is None:
    raise SystemExit("could not locate main activity")
print(activity.attrib["{http://schemas.android.com/apk/res/android}name"])
PY
)

MAIN_ACTIVITY_PACKAGE="${MAIN_ACTIVITY%.*}"
SHIM_PATH="$APP_MODULE_ROOT/src/main/kotlin/${MAIN_ACTIVITY_PACKAGE//./\/}/BuildConfig.kt"

if [[ "$APPLICATION_ID" != "$MAIN_ACTIVITY_PACKAGE" ]]; then
  echo "==> Injecting BuildConfig shim for package mismatch: appId=$APPLICATION_ID, activityPkg=$MAIN_ACTIVITY_PACKAGE"
  mkdir -p -- "$(dirname -- "$SHIM_PATH")"
  cat > "$SHIM_PATH" <<EOF
package $MAIN_ACTIVITY_PACKAGE

object BuildConfig {
    @JvmField
    val DEBUG: Boolean = $APPLICATION_ID.BuildConfig.DEBUG
}
EOF
fi

GRADLE_TASK=assembleDebug
APK_PATH="$APP_MODULE_ROOT/build/outputs/apk/debug/app-debug.apk"
if [[ "$BUILD_PROFILE" == "release" ]]; then
  GRADLE_TASK=assembleRelease
  APK_PATH="$APP_MODULE_ROOT/build/outputs/apk/release/app-release.apk"
fi

echo "==> Running ./gradlew $GRADLE_TASK"
(
  cd "$ANDROID_APP_ROOT"
  chmod +x ./gradlew
  ./gradlew "$GRADLE_TASK"
)

if [[ ! -f "$APK_PATH" ]]; then
  echo "Expected APK at $APK_PATH" >&2
  exit 1
fi

echo "APK: $APK_PATH"

if [[ "$INSTALL_APK" == true ]]; then
  ADB_SERIAL="$(resolve_adb_target)"
  ADB_ARGS=()
  if [[ -n "${ADB_SERIAL:-}" ]]; then
    ADB_ARGS+=( -s "$ADB_SERIAL" )
  fi
  echo "==> Installing APK"
  adb "${ADB_ARGS[@]}" install -r "$APK_PATH"
fi

if [[ "$LAUNCH_APP" == true ]]; then
  ADB_SERIAL="$(resolve_adb_target)"
  ADB_ARGS=()
  if [[ -n "${ADB_SERIAL:-}" ]]; then
    ADB_ARGS+=( -s "$ADB_SERIAL" )
  fi
  echo "==> Launching $APPLICATION_ID/$MAIN_ACTIVITY"
  adb "${ADB_ARGS[@]}" shell am start -n "$APPLICATION_ID/$MAIN_ACTIVITY"
fi
