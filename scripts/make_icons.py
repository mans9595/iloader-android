#!/usr/bin/env python3
"""Regenerates every app icon from src/side-installer.svg.

Needs `rsvg-convert` (librsvg) and Pillow. Run from anywhere:
    python3 scripts/make_icons.py
"""

import io
import pathlib
import re
import shutil
import subprocess
import sys

from PIL import Image

ROOT = pathlib.Path(__file__).resolve().parent.parent
SOURCE = ROOT / "src" / "side-installer.svg"
ICONS = ROOT / "src-tauri" / "icons"

# Android adaptive icons: 108dp layers, of which only the central 66dp is always visible.
DENSITIES = {"mdpi": 1, "hdpi": 1.5, "xhdpi": 2, "xxhdpi": 3, "xxxhdpi": 4}


def render(svg: str, size: int) -> Image.Image:
    png = subprocess.run(
        ["rsvg-convert", "-w", str(size), "-h", str(size)],
        input=svg.encode(),
        capture_output=True,
        check=True,
    ).stdout
    return Image.open(io.BytesIO(png)).convert("RGBA")


def layer_svgs(svg: str) -> tuple[str, str]:
    """Splits the icon into a background (gradient) and a foreground (glyph) for Android."""
    body = svg.split(">", 1)[1].rsplit("</svg>", 1)[0]
    defs = re.search(r"<defs>.*?</defs>", body, re.S).group(0)
    glyph = re.sub(r"<defs>.*?</defs>|<rect width=\"1024\"[^>]*/>", "", body, flags=re.S)
    head = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024">'
    background = f'{head}{defs}<rect width="1024" height="1024" fill="url(#bg)"/></svg>'
    # Shrink the glyph so it sits inside the adaptive icon's safe zone.
    foreground = f'{head}<g transform="translate(512 512) scale(0.8) translate(-512 -512)">{glyph}</g></svg>'
    return background, foreground


def round_mask(img: Image.Image) -> Image.Image:
    from PIL import ImageDraw

    mask = Image.new("L", img.size, 0)
    ImageDraw.Draw(mask).ellipse((0, 0, img.size[0] - 1, img.size[1] - 1), fill=255)
    out = Image.new("RGBA", img.size, (0, 0, 0, 0))
    out.paste(img, (0, 0), mask)
    return out


def main() -> None:
    if not shutil.which("rsvg-convert"):
        sys.exit("rsvg-convert is required")
    svg = SOURCE.read_text()
    background, foreground = layer_svgs(svg)

    # Desktop / store icons: regenerate each existing PNG at its current size.
    for png in [*ICONS.glob("*.png"), *(ICONS / "ios").glob("*.png")]:
        size = Image.open(png).size[0]
        render(svg, size).save(png)
    big = render(svg, 1024)
    big.save(ICONS / "icon.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
    big.save(ICONS / "icon.icns")
    public = ROOT / "public"
    public.mkdir(exist_ok=True)
    (public / "side-installer.svg").write_text(svg)

    # Android launcher icons.
    android = ICONS / "android"
    for name, scale in DENSITIES.items():
        folder = android / f"mipmap-{name}"
        folder.mkdir(parents=True, exist_ok=True)
        layer = round(108 * scale)
        legacy = round(48 * scale)
        render(foreground, layer).save(folder / "ic_launcher_foreground.png")
        render(background, layer).save(folder / "ic_launcher_background.png")
        render(svg, legacy).save(folder / "ic_launcher.png")
        flat = render(background, legacy)
        flat.alpha_composite(render(foreground, legacy))
        round_mask(flat).save(folder / "ic_launcher_round.png")

    (android / "mipmap-anydpi-v26" / "ic_launcher.xml").write_text(
        '<?xml version="1.0" encoding="utf-8"?>\n'
        '<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">\n'
        '  <background android:drawable="@mipmap/ic_launcher_background"/>\n'
        '  <foreground android:drawable="@mipmap/ic_launcher_foreground"/>\n'
        "</adaptive-icon>\n"
    )
    print("icons regenerated")


if __name__ == "__main__":
    main()
