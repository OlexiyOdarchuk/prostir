#!/usr/bin/env python3
"""Meta Horizon Store art for Prostir.

    python3 packaging/store/make_art.py CAPTURE.jpg OUT_DIR

CAPTURE is a 16:9 capture from the headset (the background of every cover). The covers
carry only the exact title, as the store requires: no tagline, badge or other text.
Sizes follow https://developers.meta.com/horizon/resources/asset-guidelines/.
Needs Pillow and ImageMagick (`magick`, to draw the SVG icon).
"""

import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

ROOT = Path(__file__).resolve().parents[2]
ICON_SVG = (ROOT / "packaging/prostir.svg").read_text()
FONT = ROOT / "client/assets/NotoSans-Medium.ttf"
TITLE = "Prostir"
BG = (22, 26, 34)  # the icon's own background, #161a22


def svg_png(svg: str, size: int) -> Image.Image:
    with tempfile.TemporaryDirectory() as d:
        src, out = Path(d) / "i.svg", Path(d) / "i.png"
        src.write_text(svg)
        subprocess.run(
            ["magick", "-background", "none", "-density", "1200", str(src),
             "-resize", f"{size}x{size}", str(out)],
            check=True, capture_output=True,
        )
        return Image.open(out).convert("RGBA")


def glyph(size: int) -> Image.Image:
    """The three screens without the rounded tile behind them."""
    svg = "\n".join(l for l in ICON_SVG.splitlines() if 'rx="14"' not in l)
    return svg_png(svg, size)


def backdrop(capture: Image.Image, w: int, h: int) -> Image.Image:
    """The capture, cropped to fill, darkened towards the centre so the title reads.
    Only its upper part is used: the monitors and the sky, not whatever is open on the
    laptop screen at the bottom."""
    src = capture.convert("RGB")
    src = src.crop((0, 0, src.width, int(src.height * 0.72)))
    scale = max(w / src.width, h / src.height)
    src = src.resize((round(src.width * scale), round(src.height * scale)), Image.LANCZOS)
    left, top = (src.width - w) // 2, (src.height - h) // 2
    img = src.crop((left, top, left + w, top + h)).filter(ImageFilter.GaussianBlur(radius=max(w, h) / 900))
    shade = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    d = ImageDraw.Draw(shade)
    # A soft dark band behind the title, strongest in the middle.
    for i in range(h):
        t = max(0.0, 1 - abs(i - h / 2) / (h * 0.32))
        d.line([(0, i), (w, i)], fill=(*BG, int(40 + 170 * t * t)))
    return Image.alpha_composite(img.convert("RGBA"), shade)


def title_block(height: int) -> Image.Image:
    """Icon glyph and the title side by side, `height` tall, transparent."""
    font = ImageFont.truetype(str(FONT), int(height * 0.78))
    tb = font.getbbox(TITLE)
    tw, th = tb[2] - tb[0], tb[3] - tb[1]
    g = glyph(height)
    gap = int(height * 0.18)
    block = Image.new("RGBA", (g.width + gap + tw, height), (0, 0, 0, 0))
    block.alpha_composite(g, (0, 0))
    d = ImageDraw.Draw(block)
    d.text((g.width + gap - tb[0], (height - th) // 2 - tb[1]), TITLE, font=font, fill=(240, 242, 247, 255))
    return block


def cover(capture: Image.Image, w: int, h: int, title_share: float) -> Image.Image:
    img = backdrop(capture, w, h)
    # The title stays inside the inner 80 % (the store's safe area).
    block = title_block(int(h * title_share))
    max_w = int(w * 0.8)
    if block.width > max_w:
        block = block.resize((max_w, int(block.height * max_w / block.width)), Image.LANCZOS)
    img.alpha_composite(block, ((w - block.width) // 2, (h - block.height) // 2))
    return img.convert("RGB")


def main() -> None:
    capture = Image.open(sys.argv[1])
    out = Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    covers = {
        "hero-cover-3000x900.png": (3000, 900, 0.30),
        "cover-landscape-2560x1440.png": (2560, 1440, 0.20),
        "cover-square-1440x1440.png": (1440, 1440, 0.17),
        "cover-portrait-1008x1440.png": (1008, 1440, 0.13),
        "mini-landscape-1080x360.png": (1080, 360, 0.36),
        "trailer-cover-2560x1440.png": (2560, 1440, 0.20),
    }
    for name, (w, h, share) in covers.items():
        cover(capture, w, h, share).save(out / name)

    # Logo: transparent, the block filling the frame minus 20 % top/bottom, 10 % sides.
    block = title_block(1440 * 6 // 10)
    logo = Image.new("RGBA", (int(block.width / 0.8), 1440), (0, 0, 0, 0))
    logo.alpha_composite(block, ((logo.width - block.width) // 2, (1440 - block.height) // 2))
    logo.save(out / f"logo-{logo.width}x1440.png")

    # Icon: solid, square corners, no transparency.
    icon = Image.new("RGBA", (512, 512), (*BG, 255))
    g = glyph(512)
    icon.alpha_composite(g, (0, 0))
    icon.convert("RGB").save(out / "icon-512x512.png")
    for p in sorted(out.glob("*.png")):
        im = Image.open(p)
        print(f"{p.name}: {im.size[0]}x{im.size[1]} {im.mode}")


if __name__ == "__main__":
    main()
