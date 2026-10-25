#!/usr/bin/env python3
"""Write every copy of the Aerona mark that the product serves, from one file of art.

The mark lives once, at `artwork/aerona.png` — a square white tile with the glyph centred in
it. Everything a browser or an OS actually asks for is derived from that: favicons at three
sizes, an apple touch icon, the manifest's 192/512 pair, and a transparent version of the
glyph alone in two inks for the panel's own header (light ink for the dark theme, dark ink for
the light one, chosen by Tailwind's `dark:` variant rather than by JavaScript).

Why a script and not committed PNGs: an icon that is hand-made in a graphics editor is a file
nobody can regenerate, and a product with both themes and a manifest has seven of them. One
command, `python3 scripts/make-icons.py`, is also the only thing that keeps the *maskable*
variant honest — a maskable icon must survive a circular crop, which is a measurement, not an
eyeball.

Nothing here needs a build: `dashboard/public/` is copied verbatim by Vite, so the panel can be
rebuilt without a raster toolchain. Pillow is only needed to *change* the art.
"""
from __future__ import annotations

import sys
from pathlib import Path

try:
    from PIL import Image, ImageChops
except ImportError:  # pragma: no cover - a clear failure beats a traceback
    sys.exit("this script needs Pillow: python3 -m pip install pillow")

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "artwork" / "aerona.png"
OUT = ROOT / "dashboard" / "public"

# Sizes the panel and its install surface ask for, by name.
TILE_SIZES = {"favicon-16.png": 16, "favicon-32.png": 32, "favicon-48.png": 48}
BRAND_SIZES = {"aerona-192.png": 192, "aerona-512.png": 512}
APPLE_SIZE = 180
# The glyph on its own, for the header: the art's own padding is generous enough that the mark
# stays inside the safe circle a maskable icon is allowed to be cropped to.
MASKABLE = "aerona-maskable-512.png"
MARK_SIZES = {"aerona-mark.png": 128, "aerona-mark-light.png": 128}

LIGHT_INK = (255, 255, 255, 255)  # on the dark theme's field
DARK_INK = (23, 23, 23, 255)  # on the light theme's field -- `--background` at L=1


def load() -> Image.Image:
    if not SOURCE.is_file():
        sys.exit(f"missing the source art: {SOURCE}")
    return Image.open(SOURCE).convert("RGBA")


def tile(im: Image.Image, size: int) -> Image.Image:
    """The whole square, resized onto a white field. Favicons and the manifest want this."""
    return im.resize((size, size), Image.Resampling.LANCZOS).convert("RGBA")


def glyph_box(im: Image.Image) -> tuple[int, int, int, int]:
    """Tight box of the artwork, found as "anything that is not the background"."""
    gray = im.convert("L")
    mask = ImageChops.invert(gray).point(lambda p: 255 if p > 8 else 0)
    box = mask.getbbox()
    if not box:
        sys.exit(f"{SOURCE} has no non-white pixels in it — nothing to crop")
    return box


def mark(im: Image.Image, size: int, ink: tuple[int, int, int, int]) -> Image.Image:
    """Just the glyph, recoloured to a flat ink on transparency.

    The art is a dark silhouette on white, so the ink is a *fill*, not a tint: alpha comes from
    how far a pixel is from the background (an inverted grey ramp — white is nothing, the glyph is
    everything), and one colour is pasted through it. That is what makes the same shape read
    correctly on both themes instead of carrying a white box with it, and it is also why an
    anti-aliased edge stays smooth at 16px.
    """
    crop = im.crop(glyph_box(im))
    gray = crop.convert("L")
    # Scale the coverage so the art's own solid fill reads as fully opaque: the glyph is a dark
    # grey on white, so `255 - grey` tops out below 255 and a naive copy makes the mark look
    # washed out on a dark field. `lo` is that fill's level, measured, not guessed.
    lo = gray.getextrema()[0]  # the ink's own level, measured off this image
    span = max(1, 255 - lo)
    alpha = ImageChops.invert(gray).point(lambda p: min(255, p * 255 // span))
    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    # Fit the glyph to ~72% of the square, centred: a header logo wants breathing room, and the
    # same file doubles as a maskable icon where 20% of each edge can be cropped away.
    target = round(size * 0.72)
    scale = target / max(crop.size)
    w, h = max(1, round(crop.size[0] * scale)), max(1, round(crop.size[1] * scale))
    glyph = Image.new("RGBA", (w, h), ink)
    glyph.putalpha(alpha.resize((w, h), Image.Resampling.LANCZOS))
    out.alpha_composite(glyph, ((size - w) // 2, (size - h) // 2))
    return out


def save(im: Image.Image, name: str) -> int:
    OUT.mkdir(parents=True, exist_ok=True)
    path = OUT / name
    im.save(path, format="PNG", optimize=True)
    return path.stat().st_size


def main() -> int:
    im = load()
    written: list[str] = []
    for name, size in {**TILE_SIZES, **BRAND_SIZES}.items():
        written.append(f"{name} {size}px {save(tile(im, size), name):,} B")
    written.append(f"apple-touch-icon.png {APPLE_SIZE}px {save(tile(im, APPLE_SIZE), 'apple-touch-icon.png'):,} B")
    written.append(f"{MASKABLE} {save(tile(im, 512), MASKABLE):,} B")
    for name in MARK_SIZES:
        ink = DARK_INK if name == "aerona-mark.png" else LIGHT_INK
        written.append(f"{name} {save(mark(im, MARK_SIZES[name], ink), name):,} B")
    print("wrote dashboard/public/")
    for line in written:
        print("  ", line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
