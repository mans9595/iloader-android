# iloader for Android (unofficial)

An unofficial Android port of [iloader](https://github.com/nab138/iloader) by nab138
(official site: <https://iloader.app>). It is not affiliated with or endorsed by the
iloader project.

Plug an iPhone or iPad into an Android phone with a data cable, sign in with an Apple ID,
and install SideStore or LiveContainer+SideStore — no computer needed. The anisette server
can be chosen in the app's settings, including a custom one.

## How it differs from iloader

Desktop iloader talks to the iPhone through the system's usbmuxd / Apple Mobile Device
service. Android has neither, so this port adds:

- `src-tauri/src/android_usbmuxd.rs` — an in-process usbmuxd: the USB multiplexing
  protocol over Android's USB host API, exposed to the rest of the app on a private socket.
- `android-overlay/MainActivity.kt` — USB permission handling; hands the opened device to
  the Rust side.
- Lockdown pairing ("Trust This Computer?") done by the app itself, with pair records kept
  in the app's private storage.
- `vendor/isideload` — isideload with TLS verification that works on Android.

Apple ID passwords are not stored on Android (there is no keyring backend), so you sign in
each time the app starts.

## Building

Every push to `main` builds an APK with GitHub Actions; download it from the run's
artifacts. Locally: `bun install`, `bun tauri android init`,
`python3 scripts/patch_android_project.py`, `bun tauri android build --apk --target aarch64`.

## Status

Experimental. The code was written with AI assistance and has had little testing on real
hardware. Requirements: Android 7+ with USB host (OTG) support and a cable that carries
data (USB-C to Lightning, or USB-C to USB-C).

## Licenses

iloader and isideload are MIT licensed, see `LICENSE` and `vendor/isideload/LICENSE`.
The iloader name and artwork are covered by `LICENSE-BRANDING`.
