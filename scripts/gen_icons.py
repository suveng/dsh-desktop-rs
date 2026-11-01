#!/usr/bin/env python3
"""生成应用图标（不依赖设计稿：几何图形即可，保证任何分辨率都清晰）。

输出：packaging/linux/icons/dsh-desktop-{128,256,512}.png
"""

from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent.parent / "packaging" / "linux" / "icons"

BG = (22, 23, 26, 255)          # 与 skeleton/failure 页同色
MARK = (124, 196, 248, 255)     # 与菜单/链接的强调色一致
DIM = (61, 63, 70, 255)


def draw(size: int) -> Image.Image:
    scale = 8  # 超采样后缩小，边缘更干净
    s = size * scale
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    radius = int(s * 0.22)
    d.rounded_rectangle([0, 0, s - 1, s - 1], radius=radius, fill=BG)

    # 两条弧线代表「对话往返」，中间留白。
    w = max(scale * 2, int(s * 0.055))
    d.arc([s * 0.20, s * 0.26, s * 0.80, s * 0.74], start=200, end=340, fill=MARK, width=w)
    d.arc([s * 0.20, s * 0.26, s * 0.80, s * 0.74], start=20, end=160, fill=DIM, width=w)

    # 中心的一个方点，呼应「agent 的一条输入」。
    r = int(s * 0.075)
    c = s // 2
    d.rounded_rectangle([c - r, c - r, c + r, c + r], radius=int(r * 0.4), fill=MARK)

    return img.resize((size, size), Image.LANCZOS)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for size in (128, 256, 512):
        path = OUT / f"dsh-desktop-{size}.png"
        draw(size).save(path)
        print(f"wrote {path}")


if __name__ == "__main__":
    main()
