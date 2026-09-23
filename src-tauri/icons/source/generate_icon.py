#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
DockerDeploy SSH 应用图标源生成器(第三十五批:图标品牌化)

设计语言(与全站 ark 契约一致,勿擅自改色):
  墨底 #080a0b 直角满出血 + 纸白 #f4f6f6 容器方框 + 信号青 #18d1ff 上行箭头。
  母题 = 把本地镜像(容器)送出去:箭头自方框内升起、跨过顶边,箭头浮在顶边上方。

用法(需 Python 3 + Pillow;仅生成"源",不生成 ico/icns 图标集):
    python src-tauri/icons/source/generate_icon.py
    npx tauri icon src-tauri/icons/source/icon-1024.png
第二条命令会重写 src-tauri/icons/ 下的 32x32.png / 128x128.png /
128x128@2x.png / icon.png / icon.ico / icon.icns / Square*Logo.png /
StoreLogo.png(不删本目录,只覆盖同名输出)。

注意:`tauri icon` 会顺带产出本项目用不到的移动端与 64px 产物,完事请清掉:
    rm -rf src-tauri/icons/android src-tauri/icons/ios src-tauri/icons/64x64.png
(本项目 Windows 桌面专用,打包引用见 tauri.conf.json 的 bundle.icon;
 exe / 窗口 / 托盘图标都取 bundle.icon 里第一个 .ico,即 icons/icon.ico)

改形状只需改下面 GEOMETRY 里的数字;超采样倍率 SUPERSAMPLE 保证边缘抗锯齿。
"""
import os

from PIL import Image, ImageDraw

# ---- 色值(取自 ui/style.css 的 :root[data-ark-theme="ark"])----
INK = (8, 10, 11)        # --ark-ink
PAPER = (244, 246, 246)  # --ark-paper
SIGNAL = (24, 209, 255)  # --ark-signal

# ---- 几何(1024 坐标系;B2b 定稿:箭头浮起 + 粗箭杆)----
SIZE = 1024
SUPERSAMPLE = 4

BOX_X0, BOX_Y0 = 224, 380
BOX_X1, BOX_Y1 = 800, 860
BOX_STROKE = 84

ARROW_CX = 512
ARROW_TIP_Y = 164
ARROW_HEAD_HALF_W = 184
ARROW_HEAD_BASE_Y = 308
ARROW_SHAFT_W = 112
ARROW_TAIL_Y = 780

SIGNAL_HEX = '#%02x%02x%02x' % SIGNAL
PAPER_HEX = '#%02x%02x%02x' % PAPER


def draw_png():
    """画 1024×1024 位图(PIL,超采样后缩回)。"""
    n = SIZE * SUPERSAMPLE
    k = SUPERSAMPLE
    img = Image.new('RGB', (n, n), INK)
    d = ImageDraw.Draw(img)

    # 容器方框(PIL 的 rectangle 描边自外沿向内,故外沿即给定坐标)
    d.rectangle([BOX_X0 * k, BOX_Y0 * k, BOX_X1 * k, BOX_Y1 * k],
                outline=PAPER, width=BOX_STROKE * k)

    # 上行箭头(尖 → 头右 → 颈右 → 尾右 → 尾左 → 颈左)
    half_shaft = ARROW_SHAFT_W // 2
    pts = [
        (ARROW_CX - ARROW_HEAD_HALF_W, ARROW_HEAD_BASE_Y),
        (ARROW_CX, ARROW_TIP_Y),
        (ARROW_CX + ARROW_HEAD_HALF_W, ARROW_HEAD_BASE_Y),
        (ARROW_CX + half_shaft, ARROW_HEAD_BASE_Y),
        (ARROW_CX + half_shaft, ARROW_TAIL_Y),
        (ARROW_CX - half_shaft, ARROW_TAIL_Y),
        (ARROW_CX - half_shaft, ARROW_HEAD_BASE_Y),
    ]
    d.polygon([(x * k, y * k) for x, y in pts], fill=SIGNAL)

    return img.resize((SIZE, SIZE), Image.LANCZOS)


def draw_svg():
    """同一几何的矢量版(SVG 描边居中,故矩形按 stroke/2 内缩以对齐位图)。"""
    half = BOX_STROKE / 2.0
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" width="%d" height="%d" '
        'viewBox="0 0 %d %d" role="img" aria-label="DockerDeploy SSH">\n'
        '  <rect width="%d" height="%d" fill="%s"/>\n'
        '  <rect x="%g" y="%g" width="%g" height="%g" fill="none" stroke="%s" stroke-width="%d"/>\n'
        '  <path d="M %d %d L %d %d L %d %d L %d %d L %d %d L %d %d L %d %d Z" fill="%s"/>\n'
        '</svg>\n'
    ) % (
        SIZE, SIZE, SIZE, SIZE,
        SIZE, SIZE, '#%02x%02x%02x' % INK,
        BOX_X0 + half, BOX_Y0 + half, BOX_X1 - BOX_X0 - BOX_STROKE, BOX_Y1 - BOX_Y0 - BOX_STROKE,
        PAPER_HEX, BOX_STROKE,
        ARROW_CX - ARROW_HEAD_HALF_W, ARROW_HEAD_BASE_Y,
        ARROW_CX, ARROW_TIP_Y,
        ARROW_CX + ARROW_HEAD_HALF_W, ARROW_HEAD_BASE_Y,
        ARROW_CX + ARROW_SHAFT_W // 2, ARROW_HEAD_BASE_Y,
        ARROW_CX + ARROW_SHAFT_W // 2, ARROW_TAIL_Y,
        ARROW_CX - ARROW_SHAFT_W // 2, ARROW_TAIL_Y,
        ARROW_CX - ARROW_SHAFT_W // 2, ARROW_HEAD_BASE_Y,
        SIGNAL_HEX,
    )


def main():
    out_dir = os.path.dirname(os.path.abspath(__file__))
    png_path = os.path.join(out_dir, 'icon-1024.png')
    svg_path = os.path.join(out_dir, 'icon.svg')

    draw_png().save(png_path)
    with open(svg_path, 'w', encoding='utf-8', newline='\n') as f:
        f.write(draw_svg())

    print('wrote', png_path)
    print('wrote', svg_path)
    print('next: npx tauri icon src-tauri/icons/source/icon-1024.png')


if __name__ == '__main__':
    main()
