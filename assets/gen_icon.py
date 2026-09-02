# -*- coding: utf-8 -*-
"""Generate the xxsshg app icon: dark terminal window, traffic dots,
a green `>` prompt, a bold white `SSH` wordmark and a cursor block.

Outputs (same filenames the app already embeds):
  assets/xxssh-icon.png  256x256 master (window icon, include_bytes!)
  assets/xxssh-icon.ico  multi-size exe resource icon
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


def rounded_gradient(size, radius, top, bottom, margin):
    """rounded square filled with a vertical gradient"""
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    grad = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    gd = ImageDraw.Draw(grad)
    for y in range(margin, size - margin):
        t = (y - margin) / max(1, size - 2 * margin)
        c = tuple(int(top[i] + (bottom[i] - top[i]) * t) for i in range(3)) + (255,)
        gd.line([(margin, y), (size - margin, y)], fill=c)
    mask = Image.new("L", (size, size), 0)
    md = ImageDraw.Draw(mask)
    md.rounded_rectangle([margin, margin, size - margin, size - margin], radius=radius, fill=255)
    img.paste(grad, (0, 0), mask)
    return img, mask


def fit_font(text, max_w, start=110):
    """largest font size where text fits max_w"""
    for s in range(start, 20, -2):
        f = ImageFont.truetype(FONT, s)
        w = ImageDraw.Draw(Image.new("RGBA", (8, 8))).textbbox((0, 0), text, font=f)[2]
        if w <= max_w:
            return f, w
    raise RuntimeError("no fit")


img, bg_mask = rounded_gradient(S, 58, BG_TOP, BG_BOT, margin=8)
d = ImageDraw.Draw(img)

# subtle border on the rounded square
d.rounded_rectangle([8, 8, S - 8, S - 8], radius=58, outline=BORDER, width=3, fill=None)

# terminal window card
cx0, cy0, cx1, cy1 = 30, 34, 226, 222
d.rounded_rectangle([cx0, cy0, cx1, cy1], radius=24, fill=CARD + (255,), outline=CARD_BORDER + (255,), width=2)

# traffic dots
for i, c in enumerate(DOTS):
    x = cx0 + 26 + i * 26
    d.ellipse([x - 7, cy0 + 16 - 7, x + 7, cy0 + 16 + 7], fill=c + (255,))

# main lockup: `>SSH` + cursor block, auto-fit inside the card
text = ">SSH"
gap = 10
cur_w, cur_h = 12, 44
inner_x0, inner_x1 = cx0 + 22, cx1 - 22
max_w = inner_x1 - inner_x0 - cur_w - gap
font, tw = fit_font(text, max_w)
asc, desc = font.getmetrics()
th = asc + desc
tx = inner_x0 + (max_w - tw) // 2
ty = cy0 + 44 + (cy1 - cy0 - 44 - th) // 2

# green prompt chevron, white SSH wordmark
d.text((tx, ty), ">", font=font, fill=GREEN + (255,))
w_gt = d.textbbox((0, 0), ">", font=font)[2]
d.text((tx + w_gt, ty), "SSH", font=font, fill=WHITE + (255,))

# cursor block aligned to the text's vertical center
cur_y = ty + (th - cur_h) // 2
d.rounded_rectangle([tx + tw + gap, cur_y, tx + tw + gap + cur_w, cur_y + cur_h],
                    radius=3, fill=GREEN + (255,))

img.save("assets/xxssh-icon.png")

# multi-size .ico for the exe resource
img.save("assets/xxssh-icon.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48),
                                          (64, 64), (128, 128), (256, 256)])

# preview strip for eyeballing
preview = Image.new("RGBA", (256 + 64 + 32 + 16 + 30, 256), (24, 26, 32, 255))
x = 0
for s in (256, 64, 32, 16):
    r = img.resize((s, s), Image.LANCZOS)
    preview.paste(r, (x, (256 - s) // 2), r)
    x += s + 10
preview.save("assets/preview.png")
print("ok: icon.png / icon.ico / preview.png")
