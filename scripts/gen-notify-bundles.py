#!/usr/bin/env python3
"""Build per-status HerdrNotify bundles from assets/HerdrNotify.app.

Each variant shares the terminal-notifier binary but has its own bundle id
and its own registered icon (base icon + status dot), because macOS renders
the notification's leading icon from the sender bundle's registered icon and
offers no per-notification override. Run from the repo root:

  nix-shell -p 'python3.withPackages (p: [p.pillow])' --run \
    'python3 scripts/gen-notify-bundles.py'
"""
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw

ASSETS = Path("assets")
BASE_APP = ASSETS / "HerdrNotify.app"
STATUSES = {
    "blocked": (255, 159, 10, 255),  # system orange
    "done": (48, 209, 88, 255),      # system green
}
BASE_ID = "me.aliou.herdr-cast.notify"

DOT_DIAMETER = 118
RING = 9
MARGIN = 20


def run(*args):
    subprocess.run(args, check=True)


def status_icns(base_png: Path, color, dest: Path):
    base = Image.open(base_png).convert("RGBA")
    size = base.width
    dot_layer = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(dot_layer)
    d = DOT_DIAMETER * (size // 512)
    ring = RING * (size // 512)
    margin = MARGIN * (size // 512)
    x0 = y0 = size - margin - d - ring * 2
    x1 = y1 = size - margin
    draw.ellipse([x0, y0, x1, y1], fill=(255, 255, 255, 255))
    draw.ellipse([x0 + ring, y0 + ring, x1 - ring, y1 - ring], fill=color)
    out = base.copy()
    out.alpha_composite(dot_layer)
    with tempfile.TemporaryDirectory() as tmp:
        iconset = Path(tmp) / "icon.iconset"
        iconset.mkdir()
        for s in (16, 32, 64, 128, 256, 512, 1024):
            for scale in (1, 2):
                px = s * scale
                if px > size:
                    continue
                suffix = f"_{s}x{s}" + ("@2x" if scale == 2 else "")
                out.resize((px, px), Image.LANCZOS).save(iconset / f"icon{suffix}.png")
        run("iconutil", "-c", "icns", str(iconset), "-o", str(dest))


def main():
    if not BASE_APP.is_dir():
        sys.exit(f"base bundle missing: {BASE_APP}")
    with tempfile.TemporaryDirectory() as tmp:
        base_png = Path(tmp) / "base.png"
        run("sips", "-s", "format", "png",
            str(BASE_APP / "Contents/Resources/Terminal.icns"), "--out", str(base_png))
        for name, color in STATUSES.items():
            target = ASSETS / f"HerdrNotify-{name}.app"
            if target.exists():
                shutil.rmtree(target)
            shutil.copytree(BASE_APP, target)
            status_icns(base_png, color, target / "Contents/Resources/Terminal.icns")
            bundle_id = f"{BASE_ID}.{name}"
            plist = target / "Contents/Info.plist"
            run("plutil", "-replace", "CFBundleIdentifier", "-string", bundle_id, str(plist))
            run("plutil", "-replace", "CFBundleName", "-string", f"HerdrNotify {name.title()}", str(plist))
            run("plutil", "-replace", "CFBundleDisplayName", "-string", f"HerdrNotify {name.title()}", str(plist))
            run("codesign", "--force", "--deep", "-s", "-", str(target))
            print("built", target)


if __name__ == "__main__":
    main()
