#!/usr/bin/env python3
"""生成应用图标 crates/app/assets/icon.ico（多尺寸，PNG 压缩的 ICO）。"""
from __future__ import annotations

import os
from PIL import Image, ImageDraw

S = 1024  # 超采样画布
BG_TOP = (23, 27, 34)
BG_BOT = (38, 48, 60)
ACCENT = (34, 211, 170)
TRAY = (148, 163, 184)


def rounded_mask(size: int, radius: int) -> Image.Image:
    m = Image.new("L", (size, size), 0)
    ImageDraw.Draw(m).rounded_rectangle((0, 0, size - 1, size - 1), radius=radius, fill=255)
    return m


def build() -> Image.Image:
    # 背景：竖直渐变 + 圆角
    bg = Image.new("RGB", (S, S))
    d = ImageDraw.Draw(bg)
    for y in range(S):
        t = y / (S - 1)
        d.line(
            [(0, y), (S, y)],
            fill=tuple(int(a + (b - a) * t) for a, b in zip(BG_TOP, BG_BOT)),
        )
    canvas = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    canvas.paste(bg, (0, 0), rounded_mask(S, int(S * 0.22)))

    d = ImageDraw.Draw(canvas)
    cx = S // 2
    # 下箭头：竖杆 + 三角箭头
    shaft_w = int(S * 0.085)
    shaft_top = int(S * 0.22)
    shaft_bot = int(S * 0.55)
    d.rounded_rectangle(
        (cx - shaft_w // 2, shaft_top, cx + shaft_w // 2, shaft_bot),
        radius=shaft_w // 2,
        fill=ACCENT,
    )
    head_w = int(S * 0.30)
    head_top = int(S * 0.48)
    head_bot = int(S * 0.72)
    d.polygon(
        [(cx - head_w // 2, head_top), (cx + head_w // 2, head_top), (cx, head_bot)],
        fill=ACCENT,
    )
    # 底座横线（落地）
    tray_w = int(S * 0.40)
    tray_h = int(S * 0.055)
    tray_y = int(S * 0.80)
    d.rounded_rectangle(
        (cx - tray_w // 2, tray_y, cx + tray_w // 2, tray_y + tray_h),
        radius=tray_h // 2,
        fill=TRAY,
    )
    return canvas


def main() -> None:
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    out = os.path.join(root, "crates", "app", "assets", "icon.ico")
    img = build()
    os.makedirs(os.path.dirname(out), exist_ok=True)
    img.save(
        out,
        format="ICO",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    png = out.replace(".ico", "-256.png")
    img.resize((256, 256), Image.LANCZOS).save(png, format="PNG")
    print(f"{out} ({os.path.getsize(out)} 字节)\n{png}")


if __name__ == "__main__":
    main()
