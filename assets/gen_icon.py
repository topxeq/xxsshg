# -*- coding: utf-8 -*-
"""Generate the xxsshg app icons.

Two variants of the same identity (dark rounded terminal square):
  full   — traffic dots + green `>` prompt + bold white `SSH` + cursor block
  simple — just the green `>` prompt + white cursor block, no card, no text;
           designed to stay legible at 16px (title bar / small taskbar)

Outputs (same filenames the app already embeds):
  assets/xxssh-icon.png  256x256 SIMPLE master (window icon: winit uses this
                         single image for BOTH the 16px title bar icon and the
                         taskbar icon, so it must survive a 16x downscale)
  assets/xxssh-icon.ico  per-size variants: >=64px full, <=48px simple
  assets/preview.png     eyeballing strip
"""
from PIL import Image, ImageDraw, ImageFont
import os

S = 256
FONT = r"C:\Windows\Fonts\consolab.ttf"  # Consolas Bold
if not os.path.exists(FONT):
    FONT = r"C:\Windows\Fonts\arialbd.ttf"

# palette
BG_TOP = (43, 53, 71)      # slate
BG_BOT = (15, 20, 30)      # near black
BORDER = (62, 76, 99)
CARD = (13, 18, 27)
CARD_BORDER = (45, 56, 74)
GREEN = (74, 246, 118)     # terminal green
WHITE = (233, 239, 247)
DOTS = [(255, 95, 87), (254, 188, 46), (40, 200, 64)]


def base_square():
    """256 rounded square with vertical gradient + subtle border"""
    margin, radius = 8, 58
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    grad = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    gd = ImageDraw.Draw(grad)
    for y in range(margin, S - margin):
        t = (y - margin) / (S - 2 * margin)
        c = tuple(int(BG_TOP[i] + (BG_BOT[i] - BG_TOP[i]) * t) for i in range(3)) + (255,)
        gd.line([(margin, y), (S - margin, y)], fill=c)
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [margin, margin, S - margin, S - margin], radius=radius, fill=255)
    img.paste(grad, (0, 0), mask)
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([margin, margin, S - margin, S - margin], radius=radius,
                        outline=BORDER, width=3)
    return img


def fit_font(draw, text, max_w, start):
    floor = max(6, start // 6)
    for s in range(start, floor, -2):
        f = ImageFont.truetype(FONT, s)
        w = draw.textbbox((0, 0), text, font=f)[2]
        if w <= max_w:
            return f
    raise RuntimeError("no fit")


def render_full():
    """full variant: dots + >SSH + cursor (for >=64px contexts)"""
    img = base_square()
    d = ImageDraw.Draw(img)
    cx0, cy0, cx1, cy1 = 30, 34, 226, 222
    d.rounded_rectangle([cx0, cy0, cx1, cy1], radius=24, fill=CARD + (255,),
                        outline=CARD_BORDER + (255,), width=2)
    for i, c in enumerate(DOTS):
        x = cx0 + 26 + i * 26
        d.ellipse([x - 7, cy0 + 16 - 7, x + 7, cy0 + 16 + 7], fill=c + (255,))
    text = ">SSH"
    gap, cur_w, cur_h = 10, 12, 44
    ix0, ix1 = cx0 + 22, cx1 - 22
    max_w = ix1 - ix0 - cur_w - gap
    font = fit_font(d, text, max_w, 110)
    tw = d.textbbox((0, 0), text, font=font)[2]
    asc, desc = font.getmetrics()
    th = asc + desc
    tx = ix0 + (max_w - tw) // 2
    ty = cy0 + 44 + (cy1 - cy0 - 44 - th) // 2
    d.text((tx, ty), ">", font=font, fill=GREEN + (255,))
    w_gt = d.textbbox((0, 0), ">", font=font)[2]
    d.text((tx + w_gt, ty), "SSH", font=font, fill=WHITE + (255,))
    cur_y = ty + (th - cur_h) // 2
    d.rounded_rectangle([tx + tw + gap, cur_y, tx + tw + gap + cur_w, cur_y + cur_h],
                        radius=3, fill=GREEN + (255,))
    return img


def render_simple(px):
    """simple variant: green `>` + white cursor on the bare square (small sizes).
    Rendered at 4x and downscaled for crisp anti-aliasing."""
    ss = px * 4
    margin, radius = max(1, ss * 8 // S), ss * 58 // S
    img = Image.new("RGBA", (ss, ss), (0, 0, 0, 0))
    grad = Image.new("RGBA", (ss, ss), (0, 0, 0, 0))
    gd = ImageDraw.Draw(grad)
    for y in range(margin, ss - margin):
        t = (y - margin) / (ss - 2 * margin)
        c = tuple(int(BG_TOP[i] + (BG_BOT[i] - BG_TOP[i]) * t) for i in range(3)) + (255,)
        gd.line([(margin, y), (ss - margin, y)], fill=c)
    mask = Image.new("L", (ss, ss), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [margin, margin, ss - margin, ss - margin], radius=radius, fill=255)
    img.paste(grad, (0, 0), mask)
    d = ImageDraw.Draw(img)
    if px >= 48:  # border adds definition at large sizes, eats pixels at tiny ones
        d.rounded_rectangle([margin, margin, ss - margin, ss - margin], radius=radius,
                            outline=BORDER, width=max(1, ss * 3 // S))

    # chevron (vector polyline, thick round stroke) + cursor block, centered
    gap = ss * 12 // S
    h = ss * 100 // S                      # lockup height
    stroke = max(2, ss * 24 // S)
    chev_w = ss * 55 // S
    cur_w = ss * 15 // S
    total = chev_w + gap + cur_w
    x0 = (ss - total) // 2
    y0 = (ss - h) // 2
    ymid = y0 + h // 2
    d.line([(x0, y0), (x0 + chev_w, ymid), (x0, y0 + h)],
           fill=GREEN + (255,), width=stroke, joint="curve")
    r = stroke // 2  # round end caps so the stroke ends aren't sheared
    for ex, ey in ((x0, y0), (x0, y0 + h), (x0 + chev_w, ymid)):
        d.ellipse([ex - r, ey - r, ex + r, ey + r], fill=GREEN + (255,))
    d.rounded_rectangle([x0 + chev_w + gap, y0, x0 + chev_w + gap + cur_w, y0 + h],
                        radius=max(1, ss * 3 // S), fill=WHITE + (255,))
    return img.resize((px, px), Image.LANCZOS)


full = render_full()
window_icon = render_simple(256)  # winit: ONE image for title bar + taskbar
window_icon.save("assets/xxssh-icon.png")

# per-size ico: >=64 full design, <=48 simple variant
sizes = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]
frames = {sz: (full.resize(sz, Image.LANCZOS) if sz[0] >= 64 else render_simple(sz[0]))
          for sz in sizes}
frames[(256, 256)].save("assets/xxssh-icon.ico",
                        append_images=[frames[s] for s in sizes[:-1]],
                        sizes=sizes)

# preview strip + zoomed tiny sizes for legibility check
order = [("full 256", full), ("simple 256", window_icon), ("simple 64", frames[(64, 64)]),
         ("simple 32", frames[(32, 32)]), ("simple 16", frames[(16, 16)])]
pw = sum(im.width for _, im in order) + 12 * len(order)
preview = Image.new("RGBA", (pw, 256), (24, 26, 32, 255))
x = 0
for _, im in order:
    preview.paste(im, (x, (256 - im.height) // 2), im)
    x += im.width + 12
preview.save("assets/preview.png")

# 4x nearest-neighbor zoom of the tiny sizes (what a 16px title-bar icon looks like)
zoom = Image.new("RGBA", (32 * 4 + 16 * 4 + 36, 128), (24, 26, 32, 255))
zx = 0
for sz in (32, 16):
    r = frames[(sz, sz)].resize((sz * 4, sz * 4), Image.NEAREST)
    zoom.paste(r, (zx, (128 - r.height) // 2), r)
    zx += r.width + 12
zoom.save("assets/preview-zoom.png")
print("ok: icon.png (simple) / icon.ico (per-size) / preview.png / preview-zoom.png")
