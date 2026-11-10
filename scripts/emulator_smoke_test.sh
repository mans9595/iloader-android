#!/usr/bin/env bash
# Smoke test for the APK on an Android emulator (run by CI inside android-emulator-runner).
#
# Installs the APK, launches it and checks that it stays up and that the web UI renders.
# It cannot test the iPhone side: an emulator has no USB host with an iPhone attached.
# Findings are reported as GitHub annotations; the screenshot, UI dump and logcat are
# left in ./smoke-test for upload as an artifact.
#
# Usage: emulator_smoke_test.sh path/to/app.apk

set -u
APK="$1"
PKG="com.mans.sideinstaller"
OUT="smoke-test"
mkdir -p "$OUT"
failures=0

# GitHub annotation; message may be multi-line.
annotate() {
  local level="$1" title="$2" message="$3"
  message="${message//'%'/'%25'}"
  message="${message//$'\r'/}"
  message="${message//$'\n'/'%0A'}"
  echo "::${level} title=${title}::${message}"
}

fail() {
  annotate error "$1" "$2"
  failures=$((failures + 1))
}

ui_text() {
  # Every non-empty text / content-desc in the current UI, one per line.
  adb shell uiautomator dump /sdcard/ui.xml >/dev/null 2>&1 || return 1
  adb pull /sdcard/ui.xml "$OUT/ui.xml" >/dev/null 2>&1 || return 1
  grep -oE '(text|content-desc)="[^"]+"' "$OUT/ui.xml" | sed -E 's/^[a-z-]+="//; s/"$//' | awk '!seen[$0]++'
}

adb shell getprop ro.build.version.release > "$OUT/android-version.txt"
adb shell getprop ro.product.cpu.abilist > "$OUT/abi.txt"

if ! adb install -r "$APK" > "$OUT/install.txt" 2>&1; then
  fail "Install failed" "$(cat "$OUT/install.txt")"
  exit 1
fi
annotate notice "Installed" "APK installed on Android $(cat "$OUT/android-version.txt") ($(cat "$OUT/abi.txt"))"

adb logcat -c
adb shell am start -W -n "$PKG/.MainActivity" > "$OUT/start.txt" 2>&1

# Wait for the web UI to come up.
rendered=""
for _ in $(seq 1 24); do
  sleep 5
  if text="$(ui_text)" && grep -q "Side Installer" <<<"$text"; then
    rendered="yes"
    break
  fi
done
sleep 10  # let the first device scan finish
text="$(ui_text || true)"
printf '%s\n' "$text" > "$OUT/ui-text.txt"
adb exec-out screencap -p > "$OUT/screen.png"

pid="$(adb shell pidof "$PKG" | tr -d '\r')"
adb logcat -d > "$OUT/logcat.txt"

if [ -z "$pid" ]; then
  fail "App is not running" "The process ${PKG} is gone after launch."
fi
if [ -z "$rendered" ]; then
  fail "UI did not render" "\"Side Installer\" never appeared on screen. Visible text:
$(head -c 2500 "$OUT/ui-text.txt")"
else
  annotate notice "UI rendered" "Visible text:
$(head -c 3000 "$OUT/ui-text.txt")"
fi

crashes="$(grep -E 'FATAL EXCEPTION|panicked at|UnsatisfiedLinkError|Could not load native library|Native USB entry point missing|AndroidRuntime: Process: '"$PKG" "$OUT/logcat.txt" | head -c 3000)"
if [ -n "$crashes" ]; then
  fail "Crash in logcat" "$crashes"
fi

# Lines from the app itself, for diagnosis (web console, our USB code, Tauri, Rust).
app_lines="$(grep -iE "SideInstallerUsb|Tauri|RustStdoutStderr|chromium.*CONSOLE|${PKG}" "$OUT/logcat.txt" | grep -vE 'ActivityManager|PackageManager' | tail -n 40 | head -c 3500)"
if [ -n "$app_lines" ]; then
  annotate notice "App log" "$app_lines"
fi

if [ "$failures" -gt 0 ]; then
  exit 1
fi
annotate notice "Smoke test passed" "Installed, launched, stayed up and rendered the UI."
