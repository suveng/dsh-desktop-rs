#!/usr/bin/env python3
"""逐像素对比两张截图，用于原生界面的视觉回归。

用法： compare_shots.py <baseline.png> <candidate.png> [tolerance]
退出码：0 一致；1 差异超过容差；2 无法比较（尺寸不同或缺文件）。

为什么不用 SSIM：这里比较的是我们自己画的原生页面，字体与布局由固定的 GTK 主题决定，
逐像素 + 小容差已经足够；引入 SSIM 只会让「差多少算差」变得难以解释。
"""

import sys
from pathlib import Path

from PIL import Image, ImageChops

# 单个通道差超过这个值才算「不一样」，滤掉抗锯齿的抖动。
PIXEL_DELTA = 8


def main(argv: list[str]) -> int:
    if len(argv) < 3:
        print("用法: compare_shots.py <baseline> <candidate> [tolerance]", file=sys.stderr)
        return 2

    baseline_path, candidate_path = Path(argv[1]), Path(argv[2])
    tolerance = float(argv[3]) if len(argv) > 3 else 0.01

    for path in (baseline_path, candidate_path):
        if not path.exists():
            print(f"缺少文件：{path}", file=sys.stderr)
            return 2

    baseline = Image.open(baseline_path).convert("RGB")
    candidate = Image.open(candidate_path).convert("RGB")

    if baseline.size != candidate.size:
        print(
            f"尺寸不同：基线 {baseline.size} vs 候选 {candidate.size}",
            file=sys.stderr,
        )
        return 2

    diff = ImageChops.difference(baseline, candidate)
    differing = sum(
        1 for pixel in diff.getdata() if max(pixel) > PIXEL_DELTA
    )
    total = baseline.size[0] * baseline.size[1]
    ratio = differing / total

    print(f"差异像素 {differing}/{total} = {ratio:.4%}（容差 {tolerance:.2%}）")
    return 0 if ratio <= tolerance else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
