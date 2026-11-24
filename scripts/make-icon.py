#!/usr/bin/env python3
"""Render SplitGrid Lite's app icon: the full app's white "S" (its Icon Composer
layer, copied to icon/logo-1024.png) on a lighter
grey tile, laid out on Apple's macOS icon grid, written as icon/AppIcon.icns.

The full app compiles a Liquid Glass icon with Xcode's actool; this needs only
Pillow and iconutil. Run: python3 scripts/make-icon.py
"""
import os, shutil, subprocess, sys, tempfile
from PIL import Image, ImageChops, ImageDraw, ImageFilter

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LOGO = os.path.join(ROOT, "icon", "logo-1024.png")
OUT = os.path.join(ROOT, "icon", "AppIcon.icns")

S = 1024
TILE = 824          # Apple's macOS grid: an 824px tile centred in 1024
OFF = (S - TILE) // 2
TOP, BOTTOM = (0x4a, 0x4a, 0x4c), (0x36, 0x36, 0x38)   # the full app's tile is #1a1a1a


def squircle(size, n=5.0):
    """A superellipse mask, close to macOS's continuous-corner tile."""
    big = size * 4
    m = Image.new("L", (big, big), 0)
    px = m.load()
    r = big / 2
    for y in range(big):
        dy = abs((y + 0.5 - r) / r) ** n
        if dy >= 1:
            continue
        half = r * (1 - dy) ** (1 / n)
        x0, x1 = int(round(r - half)), int(round(r + half))
        for x in range(x0, x1):
            px[x, y] = 255
    return m.resize((size, size), Image.LANCZOS)


def vgradient(size, top, bottom):
    g = Image.new("RGB", (1, size[1]))
    for y in range(size[1]):
        t = y / (size[1] - 1)
        g.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)))
    return g.resize(size)


def render():
    canvas = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    mask = squircle(TILE)

    # Soft shadow under the tile.
    shadow = Image.new("L", (S, S), 0)
    shadow.paste(mask, (OFF, OFF + 12))
    shadow = shadow.filter(ImageFilter.GaussianBlur(18)).point(lambda v: v * 0.45)
    canvas.paste(Image.new("RGBA", (S, S), (0, 0, 0, 255)), (0, 0), shadow)

    tile = vgradient((TILE, TILE), TOP, BOTTOM).convert("RGBA")
    # A faint light rim along the top edge, as the glass tile has.
    rim = ImageChops.subtract(mask, mask.transform(mask.size, Image.AFFINE, (1, 0, 0, 0, 1, -3)))
    rim = rim.filter(ImageFilter.GaussianBlur(1.5)).point(lambda v: v * 0.35)
    tile.paste(Image.new("RGBA", tile.size, (255, 255, 255, 255)), (0, 0), rim)

    # The "S": white to #f0f0f0 over the top 70%, with a soft drop shadow.
    logo = Image.open(LOGO).convert("RGBA").resize((TILE, TILE), Image.LANCZOS)
    alpha = logo.getchannel("A")
    drop = alpha.transform(alpha.size, Image.AFFINE, (1, 0, 0, 0, 1, -10)).filter(ImageFilter.GaussianBlur(14)).point(lambda v: v * 0.5)
    tile.paste(Image.new("RGBA", tile.size, (0, 0, 0, 255)), (0, 0), drop)
    stop = int(TILE * 0.7)
    fill = vgradient((1, stop), (255, 255, 255), (240, 240, 240)).resize((TILE, stop))
    face = Image.new("RGB", (TILE, TILE), (240, 240, 240))
    face.paste(fill, (0, 0))
    tile.paste(face, (0, 0), alpha)

    tile.putalpha(ImageChops.multiply(tile.getchannel("A"), mask))
    canvas.alpha_composite(tile, (OFF, OFF))
    return canvas


def main():
    if not os.path.exists(LOGO):
        sys.exit(f"missing {LOGO}")
    big = render()
    tmp = tempfile.mkdtemp()
    iconset = os.path.join(tmp, "AppIcon.iconset")
    os.mkdir(iconset)
    for pt in (16, 32, 128, 256, 512):
        for scale in (1, 2):
            px = pt * scale
            name = f"icon_{pt}x{pt}{'@2x' if scale == 2 else ''}.png"
            big.resize((px, px), Image.LANCZOS).save(os.path.join(iconset, name))
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    big.save(os.path.join(os.path.dirname(OUT), "AppIcon-1024.png"))
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", OUT], check=True)
    shutil.rmtree(tmp)
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
