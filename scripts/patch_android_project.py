#!/usr/bin/env python3
"""Adds USB host support to the Android project that `tauri android init` generates.

The generated project (src-tauri/gen/android) is not committed, so CI runs this after
`tauri android init` to install our MainActivity, the USB bits of the manifest and the launcher icons.
"""

import pathlib
import re
import shutil
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
OVERLAY = ROOT / "android-overlay"
APP_MAIN = ROOT / "src-tauri" / "gen" / "android" / "app" / "src" / "main"
EXPECTED_PACKAGE = "com.mans.sideinstaller"


def fail(message: str) -> None:
    sys.exit(f"patch_android_project: {message}")


def patch_main_activity() -> None:
    matches = list(APP_MAIN.rglob("MainActivity.kt"))
    if len(matches) != 1:
        fail(f"expected exactly one generated MainActivity.kt, found {matches}")
    target = matches[0]
    package = re.search(r"^package\s+([\w.]+)", target.read_text(), re.MULTILINE)
    # The JNI symbols in src-tauri/src/lib.rs are named after this package.
    if not package or package.group(1) != EXPECTED_PACKAGE:
        fail(f"generated package is {package.group(1) if package else None}, expected {EXPECTED_PACKAGE}")
    shutil.copyfile(OVERLAY / "MainActivity.kt", target)
    print(f"installed {target.relative_to(ROOT)}")


def patch_manifest() -> None:
    manifest = APP_MAIN / "AndroidManifest.xml"
    text = manifest.read_text()
    if "android.hardware.usb.host" in text:
        print("manifest already patched")
        return

    feature = '    <uses-feature android:name="android.hardware.usb.host" android:required="true" />\n'
    text, count = re.subn(r"(\n\s*<application\b)", "\n" + feature.rstrip("\n") + r"\1", text, count=1)
    if count != 1:
        fail("could not find <application> in the manifest")

    usb_filter = (
        "            <intent-filter>\n"
        '                <action android:name="android.hardware.usb.action.USB_DEVICE_ATTACHED" />\n'
        "            </intent-filter>\n"
        "            <meta-data\n"
        '                android:name="android.hardware.usb.action.USB_DEVICE_ATTACHED"\n'
        '                android:resource="@xml/device_filter" />\n'
    )
    text, count = re.subn(r"(\n\s*</activity>)", "\n" + usb_filter.rstrip("\n") + r"\1", text, count=1)
    if count != 1:
        fail("could not find </activity> in the manifest")

    manifest.write_text(text)
    print(f"patched {manifest.relative_to(ROOT)}")


def install_icons() -> None:
    """Copies our launcher icons over the ones the project template ships with."""
    source = ROOT / "src-tauri" / "icons" / "android"
    res = APP_MAIN / "res"
    for path in source.rglob("*"):
        if path.is_file():
            target = res / path.relative_to(source)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, target)
    print(f"installed launcher icons into {res.relative_to(ROOT)}")


def install_device_filter() -> None:
    xml_dir = APP_MAIN / "res" / "xml"
    xml_dir.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(OVERLAY / "device_filter.xml", xml_dir / "device_filter.xml")
    print(f"installed {(xml_dir / 'device_filter.xml').relative_to(ROOT)}")


if __name__ == "__main__":
    if not APP_MAIN.is_dir():
        fail(f"{APP_MAIN} does not exist; run `tauri android init` first")
    patch_main_activity()
    patch_manifest()
    install_device_filter()
    install_icons()
