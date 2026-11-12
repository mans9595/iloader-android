# Side Installer

Install SideStore on an iPhone or iPad straight from an Android phone — no computer needed.

Plug the iPhone into the Android phone with a data cable, sign in with an Apple ID, and
Side Installer downloads SideStore (or LiveContainer+SideStore), signs it, installs it and
puts the pairing file in place. The anisette server can be chosen in the app's settings,
including a custom one.

Side Installer is an unofficial Android port of [iloader](https://github.com/nab138/iloader)
by nab138. It is not affiliated with or endorsed by the iloader project.

## Download

Get `side-installer.apk` from the [latest release](https://github.com/mans9595/iloader-android/releases/latest).

## How to use

1. Install the APK on the Android phone (allow installing from unknown sources).
2. Connect the iPhone with a cable that carries data: USB-C to Lightning, or USB-C to USB-C.
3. Allow Side Installer to access the USB device when Android asks.
4. Unlock the iPhone and tap **Trust** when it asks whether to trust this device.
5. Sign in with your Apple ID and choose **Install SideStore**.

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

Every push to `main` and every pull request builds an APK with GitHub Actions; download it
from the run's artifacts (`side-installer-apk`). Locally: `bun install`,
`bun tauri android init`, `python3 scripts/patch_android_project.py`,
`bun tauri android build --apk --target aarch64`.

The icons are generated from `src/side-installer.svg` with `python3 scripts/make_icons.py`.

## Status

Experimental. The code was written with AI assistance and has had little testing on real
hardware. Requirements: Android 7+ with USB host (OTG) support, 64-bit ARM (arm64-v8a).

## Licenses

Side Installer is based on iloader and isideload, both MIT licensed: see `LICENSE` and
`vendor/isideload/LICENSE`. It does not use the iloader name or artwork as its own branding
(see `LICENSE-BRANDING`).
