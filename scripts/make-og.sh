#!/bin/sh
# Regenerate dashboard/public/og-image.jpg (1200x630) — the card a Discord or Twitter
# preview shows. Deliberately *not* part of `npm run build`: it needs ImageMagick and
# DejaVu, neither of which a CI box has to have, and the output is a committed asset.
#
#   sh scripts/make-og.sh            # writes dashboard/public/og-image.jpg
#
# It draws the same thing the product does: #0d0f12 canvas, a 48px grid, one ember glow,
# a 2px bordered card with a hard offset shadow, mono labels, no gradient text. Everything
# is specified in device pixels because a share preview has no viewport to scale against.
set -eu
cd "$(dirname "$0")/.."
OUT=dashboard/public/og-image.jpg
W=1200
H=630
FONT=/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf
REG=/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf
MONO=/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf

canvas="#0d0f12"
paper="#14171c"
line="#262c36"
ink="#eef1f5"
soft="#c3c9d3"
mute="#8e96a4"
ember="#ff6a2b"
deep="#ff9459"

# The grid, as one MVG string: 25 vertical + 13 horizontal lines, and a hand-placed
# ember, beat trying to teach an SVG renderer about letter-spacing. It is passed inline
# rather than as `-draw "@file"`, because ImageMagick's default policy.xml denies reading
# files in a `-draw`, which turns a working script into "operation not allowed by the
# security policy" on a hardened distro.
draw=''
first=1
add() { if [ "$first" = 1 ]; then draw="$1"; first=0; else draw="$draw
$1"; fi; }
x=0
while [ "$x" -le "$W" ]; do add "$(printf 'stroke rgba(255,255,255,0.035) stroke-width 1 line %s,0 %s,%s' "$x" "$x" "$H")"; x=$((x + 48)); done
y=0
while [ "$y" -le "$H" ]; do add "$(printf 'stroke rgba(255,255,255,0.035) stroke-width 1 line 0,%s %s,%s' "$y" "$W" "$y")"; y=$((y + 48)); done
add "$(printf 'fill %s stroke %s stroke-width 2 rectangle 96,168 828,492' "$paper" "$line")"
add "$(printf 'fill none stroke rgba(0,0,0,0.5) stroke-width 3 rectangle 92,172 832,496')"
add "$(printf 'fill %s stroke none rectangle 96,492 336,496' "$ember")"

magick \
  \( -size 700x700 radial-gradient:"$ember"-none -evaluate multiply 0.55 -colorspace sRGB \) \
  -size "${W}x${H}" "xc:$canvas" \
  -compose over -composite \
  \( -size "${W}x${H}" xc:none -draw "$draw" \) -compose over -composite \
  -fill "$deep" -font "$MONO" -pointsize 22 \
  -draw "text 140,232 'SELF-HOSTED  DISCORD  MODERATION'" \
  -fill "$ink" -font "$FONT" -pointsize 104 \
  -draw "text 136,336 'Aerona'" \
  -fill "$ember" -font "$FONT" -pointsize 104 -draw "text 452,336 '.'" \
  -fill "$soft" -font "$REG" -pointsize 33 \
  -draw "text 140,398 'Warns, tickets, stickies, raid defence and a'" \
  -draw "text 140,444 'rules-first text filter. One bot, one dashboard,'" \
  -draw "text 140,490 'one SQLite file — on your own server.'" \
  -fill "$mute" -font "$MONO" -pointsize 21 \
  -draw "text 884,236 'one process'" \
  -draw "text 884,278 'no cloud plan'" \
  -draw "text 884,320 'no telemetry'" \
  -draw "text 884,362 'MIT'" \
  -fill "$ember" -draw "rectangle 884,398 940,402" \
  -fill "$mute" -font "$MONO" -pointsize 19 \
  -draw "text 884,470 'python3 main.py'" \
  -quality 88 "$OUT"
printf 'wrote %s (%s bytes)\n' "$OUT" "$(stat -c%s "$OUT")"
