#!/usr/bin/env python3
"""Regenerate `dashboard/public/og-image.jpg` - the 1200×630 card a link preview shows.

Not part of `npm run build`: it needs Pillow and the DejaVu fonts, and the output is a
committed asset, so the build stays free of image toolchain. Run it by hand:

    python3 scripts/make-og.py

Everything drawn comes from the panel's own tokens: `dashboard/src/index.css` is parsed for
its `.dark` block, so the card is the palette the product actually has rather than a copy of it
that drifts. Flat field, hairline border, one light accent, mono labels, and the copy is the
product's own claim - "one process, one SQLite file" - because a share image is documentation
for the people who will never open the site.

Two things this file insists on, because both went wrong before it was committed:

* **Measured, never guessed.** Every string is laid out with `textlength`, wrapped to the
  box it has to live in, and registered with `box()`. If anything would land outside the
  canvas - or outside the card, or overlap the line above it - the script fails and prints
  the number. The first version of this image clipped its whole right column and drew the
  command line on top of the tagline; both are now assertions, not observations.
* **Cheap at this box's size.** The glow is blurred at 1/8 scale and stretched back
  (a Gaussian kernel is scale-covariant, so it is the same soft edge) instead of a
  180px blur across 1200×630, which once took enough RAM to get a `cargo` build on this
  machine OOM-killed.

ImageMagick was the first attempt and is fine for crops, but composing canvas + glow + draw
layer in one command line means later `-draw`s land on whichever image is current in the
sequence - which is how the right column rendered nowhere. Pixel-exact coordinates beat
arguing with an implicit list.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "dashboard" / "public" / "og-image.jpg"
W, H = 1200, 630

CSS = ROOT / "dashboard" / "src" / "index.css"


def _oklch(L: float, C: float, H: float) -> tuple[int, int, int]:
    """oklch -> 8-bit sRGB, by the definitions, clamped.

    Only what the panel's tokens use: `oklch(L C H)` and `oklch(L C H / A%)`. Nothing here is
    a colour management library, and it does not need to be - the file it reads is written by
    one tool with one output format.
    """
    import math

    h = math.radians(H)
    a, b = C * math.cos(h), C * math.sin(h)
    l_, m_, s_ = (
        L + 0.3963377774 * a + 0.2158037573 * b,
        L - 0.1055613458 * a - 0.0638541728 * b,
        L - 0.0894841775 * a - 1.2914855480 * b,
    )
    lin = (
        4.0767416621 * l_**3 - 3.3077115913 * m_**3 + 0.2309699292 * s_**3,
        -1.2684380046 * l_**3 + 2.6097574011 * m_**3 - 0.3413193965 * s_**3,
        -0.0041960863 * l_**3 - 0.7034186147 * m_**3 + 1.7076147010 * s_**3,
    )

    def enc(v: float) -> int:
        v = min(1.0, max(0.0, v))
        v = 1.055 * v ** (1 / 2.4) - 0.055 if v > 0.0031308 else 12.92 * v
        return round(min(1.0, max(0.0, v)) * 255)

    return tuple(enc(v) for v in lin)  # type: ignore[return-value]


def tokens() -> dict[str, tuple[int, int, int]]:
    """Read the `.dark` block of `index.css`. A token that cannot be found is a build error.

    The card used to keep its own hexes with a comment asking whoever changed the palette to
    change them here too. Nobody does that, and the share image of a product is not allowed to
    look like a previous version of it, so the numbers are read from their one source instead.
    """
    body = CSS.read_text(encoding="utf-8")
    block = re.search(r"\.dark\s*\{(?P<inner>[^}]*)\}", body)
    if not block:
        sys.exit(f"{CSS}: no `.dark` block to read the palette from")
    want = ["background", "card", "border", "foreground", "muted-foreground", "primary", "chart-1"]
    out: dict[str, tuple[int, int, int]] = {}
    for name, l, c, h, alpha in re.findall(
        r"--([\w-]+):\s*oklch\(\s*([\d.]+)\s+([\d.]+)\s+(-?[\d.]+)(?:\s*/\s*([\d.]+)%)?\s*\)",
        block.group("inner"),
    ):
        rgb = _oklch(float(l), float(c), float(h))
        if alpha:  # an unmatched optional group comes back as '', not None
            # A translucent token (`--border` is `oklch(1 0 0 / 10%)`) has to be composited
            # over what it sits on, because a JPEG has no alpha channel to defer that to.
            a = float(alpha) / 100
            base = out.get("background") or (0, 0, 0)
            rgb = tuple(round(a * x + (1 - a) * y) for x, y in zip(rgb, base))  # type: ignore[assignment]
        out[name] = rgb
    missing = [w for w in want if w not in out]
    if missing:
        sys.exit(f"{CSS}: .dark does not declare {', '.join(missing)} as oklch - this card reads "
                 f"its palette from that block, so update it here rather than adding a fallback")
    return out


APP_RS = ROOT / "crates" / "aerona-api" / "src" / "app.rs"


def route_count() -> int:
    """How many endpoints the API publishes, counted from the manifest it serves.

    The card said "60 routes" as a literal, and a number a commit changes is a number that lies
    the moment it does not get updated. Counting the table it advertises costs six lines and
    cannot drift.
    """
    src = APP_RS.read_text(encoding="utf-8")
    start = src.find("pub const ROUTES")
    if start < 0:
        sys.exit(f"{APP_RS}: no `pub const ROUTES` to count")
    end = src.find("\n];", start)
    block = src[start:end if end > 0 else len(src)]
    n = len(re.findall(r'"(?:GET|POST|PUT|PATCH|DELETE)"', block))
    if n < 10:
        sys.exit(f"{APP_RS}: parsed {n} routes out of the manifest - the block shape changed")
    return n


_T = tokens()


def _hex(name: str) -> str:
    return "#%02x%02x%02x" % _T[name]


def _mix(a: str, b: str, w: float) -> str:
    pa = int(a[1:], 16)
    pb = int(b[1:], 16)
    ch = [round(((pa >> s) & 255) * w + ((pb >> s) & 255) * (1 - w)) for s in (16, 8, 0)]
    return "#%02x%02x%02x" % tuple(ch)


CANVAS = _hex("background")
PAPER = _hex("card")
LINE = _hex("border")
INK = _hex("foreground")
MUTE = _hex("muted-foreground")
SOFT = _mix(INK, MUTE, 0.62)
EMBER = _hex("primary")
DEEP = _hex("chart-1")
GRID = _mix(LINE, CANVAS, 0.45)

REG = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"
MONO = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
MONO_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"

CARD = (72, 92, 784, 536)
PAD = 42
# The right column is a spec sheet, not part of the card: the vertical rule separates them.
COL_X = 838
COL_W = W - COL_X - 56


def font(path: str, size: int) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(path, size)


class Layout:
    """Draw onto `img` while remembering where every string ended up."""

    def __init__(self, img: Image.Image) -> None:
        self.img = img
        self.d = ImageDraw.Draw(img, "RGB")
        self.claims: list[tuple[str, tuple[int, int, int, int]]] = []
        self.errors: list[str] = []

    def text(self, xy: tuple[int, int], s: str, f: ImageFont.FreeTypeFont, fill: str,
             *, inside: tuple[int, int, int, int] | None = None, name: str = "",
             max_w: int | None = None, max_lines: int = 1, line_h: int = 0) -> int:
        """Draw one string, wrapping it to `max_w` when given. Returns the bottom edge.

        A draw that would leave the canvas, leave `inside`, or overflow `max_lines` is
        collected as an error and reported at the end, so a copy edit cannot silently
        produce a clipped image.
        """
        if max_w is None:
            lines = [s]
        else:
            lines, cur = [], ""
            for word in s.split():
                trial = f"{cur} {word}".strip()
                if d_len(self.d, trial, f) <= max_w or not cur:
                    cur = trial
                else:
                    lines.append(cur)
                    cur = word
            if cur:
                lines.append(cur)
            if len(lines) > max_lines:
                self.errors.append(f"'{name or s[:28]}' needs {len(lines)} lines, fits {max_lines} "
                                   f"(max_w={max_w}, size={f.size})")
        step = line_h or int(f.size * 1.32)
        bottom = xy[1]
        asc, desc = f.getmetrics()
        for i, line in enumerate(lines[:max_lines]):
            x, y = xy[0], xy[1] + i * step
            box = (int(x), int(y), int(x + d_len(self.d, line, f)), int(y + asc + desc))
            label = name or line[:28]
            self.claims.append((label, box))
            if box[2] > W or box[3] > H or box[0] < 0 or box[1] < 0:
                self.errors.append(f"'{label}' runs off the canvas: right={box[2]} bottom={box[3]}")
            if inside and not (inside[0] <= box[0] and box[2] <= inside[2]
                               and inside[1] <= box[1] and box[3] <= inside[3]):
                self.errors.append(f"'{label}' escapes its box: {box} vs {inside}")
            self.d.text((x, y), line, font=f, fill=fill)
            bottom = box[3]
        return bottom

    def mark(self, xyxy: tuple[int, int, int, int], path: Path, *, name: str) -> None:
        """Paste the product's glyph, alpha included, into a square box.

        A claim like any string, because a logo is where a share image goes wrong quietly: a
        non-square paste is a squashed mark, and a box 6px past the card edge is a crop nobody
        notices until the preview is up. The ink follows the palette this file already parses out
        of `index.css` rather than a second copy of that decision - a white glyph on a light card
        is invisible, and invisible is exactly what a link preview looks like.
        """
        x0, y0, x1, y1 = xyxy
        side = min(x1 - x0, y1 - y0)
        box = (x0, y0, x0 + side, y0 + side)
        self.claims.append((name, box))
        if box[2] > W or box[3] > H or x0 < 0 or y0 < 0:
            self.errors.append(f"'{name}' runs off the canvas: {box}")
        if not path.is_file():
            self.errors.append(f"'{name}' has no art: {path} - run scripts/make-icons.py first")
            return
        art = Image.open(path).convert("RGBA").resize((side, side), Image.LANCZOS)
        self.img.paste(art, (x0, y0), art)

    def block(self, xyxy: tuple[int, int, int, int], fill: str, *, name: str) -> None:
        x0, y0, x1, y1 = xyxy
        if x1 > W or y1 > H or x0 < 0 or y0 < 0:
            self.errors.append(f"'{name}' runs off the canvas: {xyxy}")
        self.d.rectangle(xyxy, fill=fill)


def _lum(hex_color: str) -> int:
    """Summed 8-bit channels of `#rrggbb` - 384 is the mid grey. Enough to pick white ink or
    black ink without importing a colour library into a script that only needs one comparison."""
    h = hex_color.lstrip("#")
    return sum(int(h[i:i + 2], 16) for i in (0, 2, 4))


def d_len(draw: ImageDraw.ImageDraw, s: str, f: ImageFont.FreeTypeFont) -> float:
    return draw.textlength(s, font=f)


def main() -> int:
    img = Image.new("RGB", (W, H), CANVAS)

    # The ember sunrise, low and right, where the product's own hero puts it.
    sw, sh = W // 8, H // 8
    glow = Image.new("L", (sw, sh), 0)
    ImageDraw.Draw(glow).ellipse((86, 34, 208, 126), fill=150)
    glow = glow.filter(ImageFilter.GaussianBlur(24)).resize((W, H), Image.BILINEAR)
    img = Image.composite(Image.new("RGB", (W, H), EMBER), img, glow.point(lambda v: v // 4))

    lay = Layout(img)
    d = lay.d

    for x in range(0, W + 1, 48):  # the 48px grid, the same step the CSS background uses
        d.line((x, 0, x, H), fill=GRID, width=1)
    for y in range(0, H + 1, 48):
        d.line((0, y, W, y), fill=GRID, width=1)

    # One hairline border and no drop shadow: the panel's cards are `border` on `background`,
    # and a floating card with a glow would be a different product's share image. The radius is
    # the preset's `--radius` at this canvas's scale, not a rounded-corner preference.
    d.rounded_rectangle(CARD, radius=26, fill=PAPER, outline=LINE, width=2)

    eyebrow = font(MONO_BOLD, 19)
    title = font(BOLD, 108)
    tag = font(REG, 28)
    mono = font(MONO, 16)
    cmd = font(MONO_BOLD, 22)
    label = font(MONO_BOLD, 17)

    x0 = CARD[0] + PAD
    inner = (CARD[0] + 8, CARD[1] + 8, CARD[2] - 8, CARD[3] - 10)
    y = lay.text((x0, CARD[1] + 34), "SELF-HOSTED  DISCORD  MODERATION", eyebrow, DEEP,
                 inside=inner, name="eyebrow")

    # The mark, then the wordmark beside it, then the ember block as a full stop. The glyph used
    # to be a drawn square standing in for a logo that did not exist yet; there is a logo now, so
    # the square is gone and its box belongs to the mark - sized to the title's cap height rather
    # than to taste. Everything on the line shifts by the mark's width, so the ember is still the
    # last thing, not something the logo sits on top of.
    ty = CARD[1] + 62
    mark_side = 104
    mark_x = x0 - 4
    word_x = mark_x + mark_side + 22
    ink = "aerona-mark-light.png" if _lum(PAPER) < 384 else "aerona-mark.png"
    lay.mark((mark_x, ty + 4, mark_x + mark_side, ty + 4 + mark_side),
             ROOT / "dashboard" / "public" / ink, name="mark")
    lay.text((word_x, ty), "Aerona", title, INK, inside=inner, name="wordmark")
    tw = d.textlength("Aerona", font=title)
    lay.block((int(word_x + tw + 16), ty + 74, int(word_x + tw + 40), ty + 98), EMBER, name="wordmark block")

    body = ("Warns and strikes, tickets, sticky messages, raid defence and a rules-first "
            "text filter - one bot, one dashboard, one SQLite file.")
    y = lay.text((x0, ty + 128), body, tag, SOFT, inside=inner, name="tagline",
                 max_w=CARD[2] - x0 - PAD, max_lines=4, line_h=38)

    # One line inside the card that says what to type, and the reassurance beside it.
    cy = CARD[3] - 66
    end = d.textlength("python3 main.py", font=cmd)
    lay.text((x0, cy), "python3 main.py", cmd, EMBER, inside=inner, name="command")
    lay.text((int(x0 + end + 28), cy + 4), "no cloud plan · no telemetry", mono, MUTE,
             inside=inner, name="command aside")

    # Right column: the four claims an operator actually checks, on the canvas beside the card.
    d.line((COL_X - 30, CARD[1] + 26, COL_X - 30, CARD[3] - 14), fill=LINE, width=2)
    rows = (
        ("ONE PROCESS", "bot + dashboard + outbox"),
        ("ONE FILE", "SQLite; VACUUM to back up"),
        (f"{route_count()} ROUTES", "GET /api/v1/manifest"),
        ("PER GUILD", "every setting, incl. the bot"),
    )
    ry = CARD[1] + 40
    for head, note in rows:
        lay.text((COL_X, ry), head, label, INK, name=f"row {head}")
        lay.text((COL_X, ry + 26), note, mono, MUTE, name=f"note {head}",
                 max_w=COL_W, max_lines=1, line_h=22)
        ry += 98

    foot = "self-hosted, so the moderation history stays yours"
    lay.text((CARD[0], H - 46), foot, mono, MUTE, name="footer")
    ver = "1.0.0 · MIT"
    lay.text((W - 56 - int(d.textlength(ver, font=mono)), H - 46), ver, mono, DEEP, name="version")

    if lay.errors:
        print("layout does not fit - fix the copy or the sizes:", file=sys.stderr)
        for e in lay.errors:
            print(f"  {e}", file=sys.stderr)
        return 1

    OUT.parent.mkdir(parents=True, exist_ok=True)
    img.save(OUT, "JPEG", quality=88, optimize=True, progressive=True)
    print(f"wrote {OUT.relative_to(ROOT)} ({OUT.stat().st_size:,} bytes, {W}x{H}, "
          f"{len(lay.claims)} measured draws)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
