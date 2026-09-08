"""Generate QuickGlass icon assets: transparent-background, neon-glowing tilted panes.

PIL-only (no native SVG/cairo dependency). Renders at high resolution with a real
Gaussian-blur glow, then downsamples to each target size for clean edges.

Run from the project root:  python build_icons.py
"""

from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

RES = Path(__file__).resolve().parent / "resources"

# Standalone PNGs kept in the repo.
PNG_SIZES = {"icon-512.png": 512, "icon-64.png": 64}
# Sizes packed into the .ico; Explorer selects the best per view.
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]

# Supersample factor: render big, blur, then shrink for anti-aliased edges.
SS = 4
# Master canvas (512 logical units matches the SVG viewBox).
BASE = 512

# Neon palette (cyan-forward so it pops on light and dark).
GLOW = (34, 211, 238)      # #22D3EE outer bloom
CORE = (103, 232, 249)     # #67E8F9 bright inner stroke
BACK_STROKE = (56, 189, 248)  # #38BDF8 back pane
GLASS_TOP = (34, 211, 238)
GLASS_BOT = (37, 99, 235)  # #2563EB


def rounded_rect_mask(w: int, h: int, rx: int) -> Image.Image:
    """Return an 'L' mask with a filled rounded rectangle covering the canvas."""
    m = Image.new("L", (w, h), 0)
    ImageDraw.Draw(m).rounded_rectangle([0, 0, w - 1, h - 1], radius=rx, fill=255)
    return m


def pane(size_px: int, cx: float, cy: float, angle: float,
         draw_body: bool, glow_factor: float,
         stroke_boost: float) -> tuple[Image.Image, Image.Image]:
    """Build a tilted pane layer and its glow layer at canvas resolution.

    Args:
        size_px: Canvas edge length in pixels (already supersampled).
        cx: Pane centre x, in the same pixel space.
        cy: Pane centre y, in the same pixel space.
        angle: Rotation in degrees (positive = counter-clockwise).
        draw_body: Whether to fill the glass gradient body (front pane only).
        glow_factor: Multiplier on the glow blur radius. Small output sizes pass
            a value well under 1 so the bloom does not swallow the shape.
        stroke_boost: Multiplier on the core stroke width. Small output sizes
            pass a value above 1 so the outline survives downsampling.

    Returns:
        A (content, glow) pair of RGBA images the size of the canvas. The glow
        image holds only the soft neon bloom; content holds body plus crisp
        strokes. Caller composites glow first, then content.
    """
    scale = size_px / BASE
    pw, ph, rx = int(200 * scale), int(260 * scale), int(16 * scale)
    stroke_core = max(1, int(5 * scale * stroke_boost))
    stroke_glow = max(2, int(12 * scale))

    # Oversized tile so rotation doesn't clip the glow.
    pad = int(60 * scale)
    tw, th = pw + pad * 2, ph + pad * 2

    # --- content tile: body + crisp core stroke ---
    content = Image.new("RGBA", (tw, th), (0, 0, 0, 0))
    box = [pad, pad, pad + pw - 1, pad + ph - 1]

    if draw_body:
        grad = Image.new("RGBA", (pw, ph), (0, 0, 0, 0))
        for y in range(ph):
            t = y / max(1, ph - 1)
            r = int(GLASS_TOP[0] + (GLASS_BOT[0] - GLASS_TOP[0]) * t)
            g = int(GLASS_TOP[1] + (GLASS_BOT[1] - GLASS_TOP[1]) * t)
            b = int(GLASS_TOP[2] + (GLASS_BOT[2] - GLASS_TOP[2]) * t)
            a = int((0.22 + (0.30 - 0.22) * t) * 255)
            for x in range(pw):
                grad.putpixel((x, y), (r, g, b, a))
        gmask = rounded_rect_mask(pw, ph, rx)
        content.paste(grad, (pad, pad), gmask)

    cd = ImageDraw.Draw(content)
    core_col = CORE if draw_body else BACK_STROKE
    core_alpha = 255 if draw_body else 200
    cd.rounded_rectangle(box, radius=rx, outline=core_col + (core_alpha,),
                         width=stroke_core)

    # --- glow tile: fat stroke, blurred ---
    glow = Image.new("RGBA", (tw, th), (0, 0, 0, 0))
    gd = ImageDraw.Draw(glow)
    glow_alpha = 230 if draw_body else 90
    gd.rounded_rectangle(box, radius=rx, outline=GLOW + (glow_alpha,),
                         width=stroke_glow)
    glow = glow.filter(ImageFilter.GaussianBlur(radius=9 * scale * glow_factor))

    # rotate both around their centre and place on full canvas
    canvas_c = Image.new("RGBA", (size_px, size_px), (0, 0, 0, 0))
    canvas_g = Image.new("RGBA", (size_px, size_px), (0, 0, 0, 0))
    rc = content.rotate(angle, resample=Image.BICUBIC, expand=True)
    rg = glow.rotate(angle, resample=Image.BICUBIC, expand=True)
    ox_c, oy_c = int(cx - rc.width / 2), int(cy - rc.height / 2)
    ox_g, oy_g = int(cx - rg.width / 2), int(cy - rg.height / 2)
    canvas_c.alpha_composite(rc, (ox_c, oy_c))
    canvas_g.alpha_composite(rg, (ox_g, oy_g))
    return canvas_c, canvas_g


def render(size: int) -> Image.Image:
    """Render the full icon at the requested output size."""
    px = size * SS
    scale = px / BASE
    img = Image.new("RGBA", (px, px), (0, 0, 0, 0))

    # Small icons: tighten the glow and thicken the stroke so the shape survives
    # downsampling. Large icons keep the full bloom. Tuned so 512 is lush and 16
    # stays a legible outline.
    if size <= 24:
        glow_factor, stroke_boost = 0.25, 2.6
    elif size <= 48:
        glow_factor, stroke_boost = 0.5, 1.8
    elif size <= 128:
        glow_factor, stroke_boost = 0.8, 1.2
    else:
        glow_factor, stroke_boost = 1.0, 1.0

    # Back pane (depth), then front pane. Angle -12deg -> +12 in PIL (CCW positive).
    back_c, back_g = pane(px, 276 * scale, 276 * scale, 12, draw_body=False,
                          glow_factor=glow_factor, stroke_boost=stroke_boost)
    front_c, front_g = pane(px, 240 * scale, 240 * scale, 12, draw_body=True,
                            glow_factor=glow_factor, stroke_boost=stroke_boost)

    # Composite order: all glows under all content.
    img.alpha_composite(back_g)
    img.alpha_composite(front_g)
    img.alpha_composite(back_c)
    img.alpha_composite(front_c)

    # Reflection highlight on the front pane.
    refl = Image.new("RGBA", (px, px), (0, 0, 0, 0))
    rd = ImageDraw.Draw(refl)
    rw, rh = int(45 * scale), int(90 * scale)
    rd.rounded_rectangle([0, 0, rw, rh], radius=int(10 * scale),
                         fill=(255, 255, 255, 26))
    refl = refl.crop((0, 0, rw + 1, rh + 1)).rotate(12, resample=Image.BICUBIC,
                                                     expand=True)
    img.alpha_composite(refl, (int(150 * scale), int(120 * scale)))

    return img.resize((size, size), Image.LANCZOS)


def main() -> None:
    """Write the standalone PNGs and the combined .ico."""
    for name, size in PNG_SIZES.items():
        render(size).save(RES / name)
        print(f"wrote {name} ({size}x{size})")

    frames = [render(s) for s in ICO_SIZES]
    # Hand every size to Pillow as its own frame. Without append_images, ICO save
    # downsamples the single largest image to each size, discarding the per-size
    # glow/stroke tuning done in render(). The base image must be the largest, or
    # _save skips every size wider than it.
    largest = max(frames, key=lambda f: f.width)
    others = [f for f in frames if f is not largest]
    largest.save(RES / "icon.ico", format="ICO",
                 sizes=[(s, s) for s in ICO_SIZES],
                 append_images=others)
    print(f"wrote icon.ico ({', '.join(str(s) for s in ICO_SIZES)})")


if __name__ == "__main__":
    main()
