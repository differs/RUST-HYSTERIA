# Hysteria Mobile (Dioxus 0.7)

Android-first MVP client shell built with Dioxus 0.7.x.

## Current scope

- paste/import `hy2://` / `hysteria2://` share URI
- edit server/auth/obfs/TLS fields
- edit CLI-compatible client bandwidth and QUIC tuning fields
- save / load / clear a local profile with Android `SharedPreferences`
- push CA files into app storage over `adb` and select them in-app
- connect / disconnect with `hysteria-core`
- run built-in download / upload speedtests
- start Android `VpnService`
- protect QUIC UDP sockets with `VpnService.protect(fd)`
- expose a local SOCKS runtime on `127.0.0.1:1080`
- bridge TUN traffic into local SOCKS via `tun2socks`
- inspect runtime status and logs
- adb launch extras for prefill + auto-connect + auto-start-VPN testing

## Not done yet

- Android file picker / SAF document import for CA files
- full process-death recovery / moving the core runtime under the Android service lifecycle
- Android UX polish around VPN permission / notification / errors
- packaging, signing, and shipping workflow

## Real-device status

Validated on a real Android 16 device (`PHP110`):

- app installs and launches
- launch extras prefill the form and are visible both in the editable fields and the `Effective config` section
- the UI exposes `Save Profile`, `Load Saved`, and `Clear Saved`
- `hysteria-core` connects successfully
- Android VPN permission flow works
- `VpnService` starts
- `tun2socks` starts from the Rust side
- runtime status shows `VPN service active = true`
- Android `dumpsys connectivity` shows `VPN CONNECTED extra: VPN:io.hysteria.mobile`
- Android DNS/default routes move onto `tun0`
- real device traffic appears on the Rust-side SOCKS/tun2socks path after VPN start
- when the remote server is stopped and restarted, the app reconnects automatically and restores the Android VPN path while the app is in the background

Current reconnect hardening covers:

- QUIC connection closure watching
- automatic reconnect with capped backoff
- local SOCKS restart after reconnect
- Android VPN/tun2socks restart after reconnect

Still not covered yet:

- full process recreation after Android kills the app process
- re-homing the core runtime into the foreground `VpnService` instead of the UI-owned Rust controller thread

## adb test launch extras

The app accepts these adb extras on launch:

- `io.hysteria.mobile.extra.SERVER`
- `io.hysteria.mobile.extra.AUTH`
- `io.hysteria.mobile.extra.OBFS_PASSWORD`
- `io.hysteria.mobile.extra.SNI`
- `io.hysteria.mobile.extra.CA_PATH`
- `io.hysteria.mobile.extra.PIN_SHA256`
- `io.hysteria.mobile.extra.BANDWIDTH_UP`
- `io.hysteria.mobile.extra.BANDWIDTH_DOWN`
- `io.hysteria.mobile.extra.QUIC_INIT_STREAM_RECEIVE_WINDOW`
- `io.hysteria.mobile.extra.QUIC_MAX_STREAM_RECEIVE_WINDOW`
- `io.hysteria.mobile.extra.QUIC_INIT_CONNECTION_RECEIVE_WINDOW`
- `io.hysteria.mobile.extra.QUIC_MAX_CONNECTION_RECEIVE_WINDOW`
- `io.hysteria.mobile.extra.QUIC_MAX_IDLE_TIMEOUT`
- `io.hysteria.mobile.extra.QUIC_KEEP_ALIVE_PERIOD`
- `io.hysteria.mobile.extra.QUIC_DISABLE_PATH_MTU_DISCOVERY`
- `io.hysteria.mobile.extra.INSECURE_TLS`
- `io.hysteria.mobile.extra.AUTO_CONNECT`
- `io.hysteria.mobile.extra.AUTO_REQUEST_VPN`
- `io.hysteria.mobile.extra.AUTO_START_VPN`

Example:

```bash
adb shell am start -n io.hysteria.mobile/dev.dioxus.main.MainActivity \
  --es io.hysteria.mobile.extra.SERVER '95.179.239.239:443' \
  --es io.hysteria.mobile.extra.AUTH 'example-auth' \
  --es io.hysteria.mobile.extra.OBFS_PASSWORD 'example-obfs' \
  --es io.hysteria.mobile.extra.BANDWIDTH_UP '100 Mbps' \
  --es io.hysteria.mobile.extra.BANDWIDTH_DOWN '500 Mbps' \
  --es io.hysteria.mobile.extra.QUIC_MAX_IDLE_TIMEOUT '30s' \
  --es io.hysteria.mobile.extra.QUIC_KEEP_ALIVE_PERIOD '10s' \
  --ez io.hysteria.mobile.extra.QUIC_DISABLE_PATH_MTU_DISCOVERY false \
  --ez io.hysteria.mobile.extra.INSECURE_TLS true \
  --ez io.hysteria.mobile.extra.AUTO_CONNECT true \
  --ez io.hysteria.mobile.extra.AUTO_REQUEST_VPN true \
  --ez io.hysteria.mobile.extra.AUTO_START_VPN true
```

## adb CA import

The app now keeps CA files under its Android app-specific cert directory and the
Nodes page can refresh/select them into `CA path`.

Push one or more PEM/CRT files:

```bash
mobile/scripts/android-push-ca.sh --device PHP110 core/internal/integration_tests/test.crt
```

List what is already installed:

```bash
mobile/scripts/android-push-ca.sh --device PHP110 --list
```

The adb target directory is:

```text
/sdcard/Android/data/io.hysteria.mobile/files/certs
```

Inside the app, open `Nodes`, enable `Show advanced`, then use the `Installed CAs`
selector under `CA path`.

## Android toolchain

Official Dioxus 0.7 mobile docs:
- https://dioxuslabs.com/learn/0.7/guides/platforms/mobile/
- https://dioxuslabs.com/learn/0.7/guides/tools/configure/

In this environment, Android checks were run with:

```bash
export ANDROID_NDK_HOME=/usr/lib/android-sdk/ndk/28.2.13676358
export CC_aarch64_linux_android=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android21-clang
export AR_aarch64_linux_android=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$CC_aarch64_linux_android
```

Then:

```bash
cargo check -p hysteria-mobile --target aarch64-linux-android
```

## Dioxus CLI

From this crate directory:

```bash
cargo install dioxus-cli --locked
cd mobile
# then use dx serve / dx build / dx bundle once the Android SDK + emulator are ready
```

## Android build workaround in this repo

Right now `dx build --android` generates a Gradle project where:

- `applicationId` is taken from `Dioxus.toml`
- Wry Kotlin sources still live under `dev.dioxus.main`
- `Logger.kt` expects a same-package `BuildConfig`

That can make the generated Android project fail during Kotlin compile.

Use the repo helper instead:

```bash
mobile/scripts/android-debug-build.sh --device PHP110 --install --launch
```

What it does:

1. runs `dx build --android`
2. detects the generated app id vs. Kotlin activity package
3. injects a tiny `BuildConfig` shim when they differ
4. runs `./gradlew assembleDebug`
5. optionally installs and launches on device

This keeps the app id at `io.hysteria.mobile` while working around the current
Dioxus 0.7 Android namespace mismatch.
