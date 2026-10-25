#!/bin/sh
# Panel-shaped self-test for a shipped install.
#
# A Pterodactyl/AMP game panel gives you exactly one forwarded TCP port, plain http, and a
# hostname typed into `AERONA_BASE_URL` (or not set at all). That combination is where the
# interesting bugs live: a redirect built from the wrong origin, a cookie the browser will
# never send back, a boot script that disagrees with the default theme in the CSS, an asset
# 404 because the static dir was guessed
# wrong. This script boots the committed binary against a copy of `dashboard/dist` and
# asserts the behaviour from the outside, over real HTTP, with curl.
#
#   sh scripts/verify-panel.sh
#
# Needs nothing but the binary and a build; run it after `npm run build` and
# `sh scripts/build-static.sh`. Override anything with the environment:
#
#   AERONA_BIN          binary to test          (default bin/aerona-linux-x86_64)
#   AERONA_DIST         dashboard build to serve (default dashboard/dist)
#   AERONA_VERIFY_HOST  named host to dial       (default node.example.net)
#   AERONA_VERIFY_PORT  port to bind             (default 27820)
#   AERONA_VERIFY_DIR   scratch directory        (default $TMPDIR/aerona-panel-check)
#
# Three design rules of this script, because a green harness that lies is worse than none:
# every assertion refuses to run against an empty response, every request carries --max-time,
# and the server is started in its own session so nothing about the calling shell's lifetime
# can decide the result.

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${AERONA_BIN:-$ROOT/bin/aerona-linux-x86_64}"
DIST="${AERONA_DIST:-$ROOT/dashboard/dist}"
D="${AERONA_VERIFY_DIR:-${TMPDIR:-/tmp}/aerona-panel-check}"
NAME="${AERONA_VERIFY_HOST:-node.example.net}"
PORT="${AERONA_VERIFY_PORT:-27820}"
# Every request is dialled on loopback and carries the host under test as a `Host:` header -
# which is precisely what a browser at that name sends. (An earlier draft of this script put
# the name in the URL and let curl resolve it, so it reached for a real public address and
# only looked correct because the sandbox loops it back. A self-test must not depend on DNS,
# and must not touch somebody's live server.)
BASE="http://127.0.0.1:$PORT"
IPORIGIN="http://127.0.0.1:$PORT"
NAMEORIGIN="http://$NAME:$PORT"
CURL="curl -sS --max-time 10"
pass=0
fail=0
skipped=0

ok() { printf '  PASS  %s\n' "$1"; pass=$((pass + 1)); }
no() { printf '  FAIL  %s\n        %s\n' "$1" "$2"; fail=$((fail + 1)); }
# A check that could not run must never look like a check that passed.
skip() { printf '  SKIP  %s\n        %s\n' "$1" "$2"; skipped=$((skipped + 1)); }

check() { # label  got  want
  if [ "$2" = "$3" ]; then ok "$1"; else no "$1" "expected [$3] got [$2]"; fi
}
has() { # label  needle  haystack
  if [ -z "$3" ]; then no "$1" "empty haystack - the request never answered, so this proves nothing"; return; fi
  case "$3" in
    *"$2"*) ok "$1" ;;
    *) no "$1" "no '$2' in: $(printf '%s' "$3" | head -c 220)" ;;
  esac
}
hasnt() { # label  needle  haystack
  if [ -z "$3" ]; then no "$1" "empty haystack"; return; fi
  case "$3" in
    *"$2"*) no "$1" "found '$2' in: $(printf '%s' "$3" | head -c 220)" ;;
    *) ok "$1" ;;
  esac
}

start() { # $@: extra .env lines, one assignment per argument (may be empty)
  stop 2>/dev/null || true
  rm -rf "$D"
  mkdir -p "$D"
  cp "$BIN" "$D/aerona" || { echo "no binary at $BIN - run: sh scripts/build-static.sh"; exit 2; }
  chmod +x "$D/aerona"
  [ -d "$DIST" ] || { echo "no dashboard build at $DIST - run: (cd dashboard && npm run build)"; exit 2; }
  cp -r "$DIST" "$D/dist"
  {
    printf 'AERONA_BOT=off\nAERONA_PORT=%s\n' "$PORT"
    printf 'AERONA_STATIC_DIR=%s/dist\nAERONA_DATA_DIR=%s/data\n' "$D" "$D"
    printf 'AERONA_DISCORD_CLIENT_ID=123456789012345678\nAERONA_DISCORD_CLIENT_SECRET=deadbeefdeadbeef\n'
    # One line per argument, so a caller can say `start "A=1" "B=2"`: an .env is read
    # line-wise, and two assignments on one line make the first value swallow the rest.
    for kv in "$@"; do printf '%s\n' "$kv"; done
  } > "$D/.env"
  # setsid: its own session, so the harness result cannot depend on whether the calling
  # shell is still around. (Started with `( ... & )` the server was reaped mid-run and every
  # assertion below it failed with ECONNRESET, which reads exactly like a product bug.)
  ( cd "$D" && setsid nohup ./aerona run > srv.log 2>&1 < /dev/null & echo "$!" > "$D/pid" )
  i=0
  while [ "$i" -lt 60 ]; do
    grep -qa "aerona-api listening" "$D/srv.log" && break
    kill -0 "$(cat "$D/pid" 2>/dev/null || echo 0)" 2>/dev/null || break
    i=$((i + 1))
    sleep 0.5
  done
  if ! grep -qa "aerona-api listening" "$D/srv.log"; then
    printf 'the server did not bind; last lines of %s/srv.log:\n' "$D"
    tail -12 "$D/srv.log"
    exit 2
  fi
}

stop() {
  if [ -f "$D/pid" ]; then
    kill -9 "$(cat "$D/pid")" 2>/dev/null || true
    rm -f "$D/pid"
  fi
}
trap 'stop' EXIT INT TERM

# $1 expect  $2 label  $3 method  $4 path  $5 Host header  $6.. extra curl args
req() {
  expect="$1"; label="$2"; method="$3"; path="$4"; hosth="$5"; shift 5
  out="$D/body"
  # shellcheck disable=SC2086
  code=$($CURL -o "$out" -w '%{http_code}' -X "$method" -H "$hosth" "$@" "$BASE$path" 2>"$D/err")
  # An empty body proves nothing went wrong for a 204 and for a redirect: both are
  # header-only by contract, and the assertions that follow read the Location. For a 200 it is
  # still the loudest signal we have, so it stays a failure there.
  if [ ! -s "$out" ]; then
    case "$code" in
      204|3??) : ;;
      *) no "$label" "$(cat "$D/err" 2>/dev/null | head -c 160) - empty body (http $code)"; return 1 ;;
    esac
  fi
  check "$label" "$code" "$expect"
  return 0
}

location() { # $1 host to send as Host:  $2.. extra curl args  -> the Location: of the sign-in bounce
  host="$1"; shift
  # shellcheck disable=SC2086
  $CURL -D - -o /dev/null -H "Host: $host" "$@" "$BASE/api/v1/auth/discord" 2>"$D/err" \
    | tr -d '\r' | awk 'tolower($1)=="location:"{print substr($0, index($0,":")+2)}'
}

printf '== under test\n'
printf '   %s\n' "$("$BIN" --version 2>&1 | head -1)"
printf '   dist %s\n   dial %s:%s\n\n' "$DIST" "$NAME" "$PORT"

printf '== an install that was never given its own name (AERONA_BASE_URL empty), reached by name\n'
start ""
req 200 "GET / is served over plain http on an unnamed host" GET / "Host: $NAME:$PORT"
root="$D/body"
doc=$(cat "$root")
has "  ...and the root *is* the dashboard, not a landing page" '<title>Aerona dashboard</title>' "$doc"
hasnt "  ...so there is no marketing hero in the first byte" 'op-hero' "$doc"
has "  ...and it is the panel frame, not a page about the panel" 'id="main"' "$doc"
hasnt "no inline body background (it would pin a reader who chose the other theme)" 'style="background-color' "$doc"
# The theme is one class on <html>, decided before the stylesheet paints — by a *file*, because
# this document's CSP is `script-src 'self'` with no hashes: an inline block is refused by the
# browser and the console says so, which is the failure the previous version of this check
# celebrated (it asserted the bootstrap was inline, and the CSP it was served under made that
# impossible). So: the link is in <head> before the stylesheet, the file is served, it reads the
# key the client writes, and nothing in the static markup pins a colour.
has "the theme is decided before first paint, from a same-origin file" 'src="/theme-boot.js"' "$doc"
# The order is the entire point of the file: after the stylesheet, the paint has already happened
# and the reader watches the wrong theme for a frame. Byte offsets, because a "does the head
# contain it" check would pass on a bootstrap linked last.
flat=$(printf '%s' "$doc" | tr -d '\n\r')
boot_at=$(printf '%s' "$flat" | grep -bo 'theme-boot.js' | head -1 | cut -d: -f1)
css_at=$(printf '%s' "$flat" | grep -bo 'rel="stylesheet"' | head -1 | cut -d: -f1)
before=$(if [ -n "$boot_at" ] && [ -n "$css_at" ] && [ "$boot_at" -lt "$css_at" ]; then echo yes; else echo no; fi)
check "  ...linked ahead of the stylesheet, not after it" "$before" "yes"
hasnt "  ...and no executable inline script for the CSP to refuse" '<script>' "$doc"
hasnt "  ...<html> carries no pinned dark class" '<html lang="en" class="dark"' "$doc"
req 200 "GET /theme-boot.js (the bootstrap itself)" GET /theme-boot.js "Host: $NAME:$PORT"
boot=$(cat "$D/body")
has "  ...reads the key the client writes" 'aerona.theme' "$boot"
has "  ...and the class that switches the palette" "classList.toggle('dark'" "$boot"
has "scrollbars and form controls follow the reader" 'name="color-scheme" content="light dark"' "$doc"
has "the browser chrome is told the light colour" 'content="#ffffff" media="(prefers-color-scheme: light)"' "$doc"
has "  ...and the dark one" 'content="#0a0a0a" media="(prefers-color-scheme: dark)"' "$doc"
# Headers of one path, lowercased and without CR: what the browser is actually handed.
hdrs() { $CURL -D - -o /dev/null -H "Host: $NAME:$PORT" "$BASE$1" | tr -d '\r' | tr 'A-Z' 'a-z'; }
heads=$(hdrs /)
hasnt "CSP does not force an https upgrade on a plain-http install" 'upgrade-insecure-requests' "$heads"
# Two header hygiene rules that only show up in the browser, so the response is the only place to
# look. (a) `Permissions-Policy` names a feature the browser does not implement and Chrome prints
# `Unrecognized feature '…' in 'Permissions-Policy' header` once per page load per name — so the
# retired ones are checked *out*. (b) `Cross-Origin-Opener-Policy` is ignored on a non-secure
# origin, and the browser logs that it is ignoring it; this panel is http by default, so the
# header would be noise and the noise is what the operator reports as "the panel is broken".
# The two features the user saw in *their* console that this repo never sent —
# `attribution-reporting`, `private-aggregation` — come from whatever sits in front of the
# install. There is nothing here for a test to fix.
policy=$(printf '%s\n' "$heads" | sed -n 's/^permissions-policy: //p')
for retired in document-domain sync-xhr ambient-light-sensor battery navigation-override execution-while interest-cohort; do
  hasnt "  permissions-policy does not name $retired" "$retired" "$policy"
done
hasnt "no cross-origin-opener-policy on an http origin (the browser ignores it and says so)" 'cross-origin-opener-policy' "$heads"
has "cross-origin-resource-policy still denies reads" 'cross-origin-resource-policy: same-origin' "$heads"
req 200 "GET /robots.txt" GET /robots.txt "Host: $NAME:$PORT"
# There is nothing to allow any more. `/` used to be a front page a crawler could be pointed
# at; it is the moderation panel now, and a panel's `/guild/<id>/moderation/` is somebody's
# warn queue.
has "  ...and keeps crawlers out of the panel" 'Disallow: /' "$(cat "$D/body")"
req 200 "GET /sitemap.xml" GET /sitemap.xml "Host: $NAME:$PORT"
hasnt "  ...and publishes no urls to index" '<url>' "$(cat "$D/body")"
# The root is the server list. Its *content* is client-side by design (a prerendered file
# cannot know who is asking), so what is checkable here is the head, the frame and the fact
# that the list is a route rather than a redirect out of a landing page.
has "the root is titled as the product, not as a page about it" '<title>Aerona dashboard</title>' "$doc"
has "  ...and tells robots to stay away" 'name="robots" content="noindex"' "$doc"
# A guild page has no file — its URL carries a server id — so the static layer has to own the
# prefix. A 404 on a refresh is a deep link that does not work, and the sign-in return target
# is exactly such a link: this is the one check that keeps `/guild/<id>/<page>/` shareable.
req 200 "GET /guild/<id>/moderation/ serves the app shell" GET /guild/123456789012345678/moderation/ "Host: $NAME:$PORT"
shell=$(cat "$D/body")
has "  ...an html document with the mount point" 'id="root"' "$shell"
has "  ...titled for the panel" 'Aerona dashboard' "$shell"
# The assertion that keeps this round's fix honest. `index.html` is the *prerendered* server list,
# so serving it for a guild URL hands React the wrong document to hydrate: the page recovers by
# discarding the HTML, and the operator's console says `Hydration failed ... #418` on every deep
# link. A shell has no prerendered content in it at all — which is what `main.tsx` looks for to
# decide between mounting and hydrating.
hasnt "  ...and none of the root page's markup (a prerendered body here is a hydration mismatch)" 'id="main"' "$shell"
has "  ...linked theme bootstrap, or a refresh flashes the wrong palette" 'src="/theme-boot.js"' "$shell"
req 200 "GET /access/ (the install-level page)" GET /access/ "Host: $NAME:$PORT"
has "  ...is the access page, prerendered with its own title" 'Access — Aerona dashboard' "$(cat "$D/body")"
# Every route the panel used to keep at the root is gone, and the deleted theme files with
# them. A 200 on any of these means a route table that grew back, or a build artefact
# somebody left in `public/`.
for gone in /docs/ /features/ /commands/ /security/ /legal/privacy/ /app/ /servers/ /moderation/ /tickets/ /theme.css; do
  code=$($CURL -o /dev/null -w '%{http_code}' --max-time 10 -H "Host: 127.0.0.1:$PORT" "$BASE$gone")
  check "GET $gone is not a page any more" "$code" "404"
done
asset=$(grep -o '/assets/index-[A-Za-z0-9_-]*[.]js' "$DIST/index.html" | head -1)
if [ -n "$asset" ]; then
  req 200 "GET $asset (hashed entry point)" GET "$asset" "Host: $NAME:$PORT"
  # Cache lifetime is a deploy contract, not a performance knob. A name containing a build digest
  # can be pinned forever *because* its content cannot change without the URL changing; a root
  # file keeps its name when it is replaced. The old rule — "any 200 that is not HTML" — pinned the
  # favicon, the manifest and the theme bootstrap for a year, which reads to whoever deploys the
  # next build as "my change did not take". So the split is asserted on live headers.
  h=$(hdrs "$asset")
  has "  $asset is cached for a year" 'cache-control: public, max-age=31536000, immutable' "$h"
  # Two contracts that a passing build does not guarantee, because both are *strings* the shipped
  # bundle has to carry and both were wrong in a build that compiled cleanly:
  #   - `/guild/<id>/<page>/` must be captured as a param. The pattern used to hold each page name
  #     literally, so `params.page` was always empty and every sidebar tab re-rendered the overview.
  #   - the settings documents the panel edits speak the model's field names. 83 keys once shipped
  #     camelCase, which made every save a 400 against a struct that reads snake_case.
else
  skip "GET the hashed entry point" "no /assets/index-*.js reference in the document"
fi
# The two contracts a passing build does not guarantee, because in both cases the thing that has to
# be true is a *string* in the shipped bundle: `/guild/<id>/<page>/` must be captured as a param (a
# literal segment matches the URL, puts nothing in `params`, and every sidebar tab re-renders the
# overview — which is the report this repo got twice), and the settings keys must be the model's
# (83 of them shipped camelCase, so every save was a 400 naming a key nobody had read). Read from
# `$DIST`, not from one HTTP response: the pages are lazy chunks, and an entry-chunk-only grep would
# "pass" for a bundle whose feature chunks were left behind.
if [ -n "$(ls "$DIST"/assets/*.js 2>/dev/null)" ]; then
  js=$(cat "$DIST"/assets/*.js | tr -d '\0')
  # Spelled as the identifiers, not as a URL: the table builds its patterns by calling
  # `guildPath(':guild', ':page')`, so the assembled string never exists as a literal, and the
  # minifier renders the arguments as backticked ones. `:page` in the bundle is the fact that the
  # segment is captured; the old table produced no such string, it produced fifteen literal paths.
  has "the client routes a guild page by its captured segment" ':page' "$js"
  has "  ...and the guild id is captured beside it" ':guild' "$js"
  has "the welcomer writes the field names the bot reads" 'welcome_ignore_bots' "$js"
  hasnt "  ...with none of the camelCase spellings the API rejects" 'welcomeEnabled' "$js"
  hasnt "  ...and none of the ones that used to 400 a save" 'strikeThreshold' "$js"
else
  skip "the built chunks carry their contracts" "no $DIST/assets/*.js to read"
fi
for revalidate in /theme-boot.js /favicon-48.png /site.webmanifest /; do
  hasnt "  $revalidate is not pinned: a replaced copy must show up on reload" 'max-age=31536000' "$(hdrs "$revalidate")"
done
# The brand is one art file and a generator — `artwork/aerona.png` through `scripts/make-icons.py`
# into `public/` — and both of its failure modes are silent: a document still pointing at the old
# `favicon.svg`, and a mark whose ink vanished because the source's white field became the shape.
# Sizes are checked by presence, since a 200 with a truncated file is what a bad pipeline leaves.
for icon in /favicon-16.png /favicon-32.png /favicon-48.png /apple-touch-icon.png /aerona-192.png /aerona-512.png /aerona-maskable-512.png; do
  req 200 "GET $icon (generated from the artwork)" GET "$icon" "Host: $NAME:$PORT"
done
has "the tab icon is a png the build produced" 'rel="icon" href="/favicon-32.png"' "$doc"
has "  ...plus a larger one for hidpi tabs" 'rel="icon" href="/favicon-48.png"' "$doc"
has "  ...and an apple-touch-icon for a home-screen tile" 'rel="apple-touch-icon" href="/apple-touch-icon.png"' "$doc"
req 200 "GET /site.webmanifest" GET /site.webmanifest "Host: $NAME:$PORT"
man=$(cat "$D/body")
has "  ...ships the maskable 512 icon, not only the bare one" '/aerona-maskable-512.png' "$man"
has "  ...and starts at the root, not at a folder the server does not route" '"start_url": "/"' "$man"
hasnt "  ...no leftover reference to a file that is not built" 'favicon.svg' "$man"
req 200 "GET /login/ (the only door)" GET /login/ "Host: $NAME:$PORT"
# The button itself only appears once /status says an OAuth application is configured, so the
# static document is checked for the promise and for the absence of the old form.
has "  ...asks you to sign in before it shows anything" 'Sign in to see your servers' "$(cat "$D/body")"
# The redirect-uri copy card lived here while `Invalid OAuth2 redirect_uri` was the open
# problem. The paste-able value is printed by `python3 main.py check`, next to the file that
# sets it, and a panel page is not where an operator wants a debug sheet.
hasnt "  ...and carries no redirect-uri diagnostic card" 'Discord needs this exact address' "$(cat "$D/body")"
hasnt "  ...and renders no pairing field" 'Pairing code' "$(cat "$D/body")"
# A cookie that authenticates nothing is the one failure a browser cannot fix from the inside:
# it is `HttpOnly`, so no script on the page can drop it, and every request keeps carrying it. So
# the 401 has to clear it, and a 401 with no cookie to blame must not send a Set-Cookie at all -
# otherwise "the install is refusing me" and "this browser is stale" become the same bug.
req 401 "GET /api/v1/guilds with no credential is refused, not rendered" GET /api/v1/guilds "Host: $NAME:$PORT"
stale=$($CURL -D - -o /dev/null -H "Host: $NAME:$PORT" -H 'cookie: aerona_session=not-a-real-session' \
  "$BASE/api/v1/guilds" | tr -d '\r' | tr 'A-Z' 'a-z')
has "a refused session cookie is dropped by the 401" 'set-cookie: aerona_session=;' "$stale"
has "  ...with the expiry that deletes it" 'max-age=0' "$stale"
has "  ...and the flags it was set with, so the deletion matches" 'samesite=lax' "$stale"
fresh=$($CURL -D - -o /dev/null -H "Host: $NAME:$PORT" "$BASE/api/v1/guilds" | tr -d '\r' | tr 'A-Z' 'a-z')
hasnt "a 401 with no cookie to blame sends no Set-Cookie" 'set-cookie' "$fresh"
req 200 "GET /api/v1/status" GET /api/v1/status "Host: $NAME:$PORT"
status=$(cat "$D/body")
has "  ...reports the OAuth block" '"discordOauth"' "$status"
has "  ...with the callback path the router mounts" '"callbackPath":"/auth/callback"' "$status"
has "  ...and names where the redirect uri came from" '"redirectUriSource"' "$status"
# An unnamed install derives the uri per request, so the JSON must NOT echo one back: that
# would turn this unauthenticated endpoint into an oracle for whatever Host a client sends.
hasnt "  ...and does not reflect the request's host as a redirect uri" '"redirectUri":"http' "$status"
# The panel's chrome has to be in the shipped stylesheet, not only in the source tree. The
# name comes from the build on disk: $doc is the first response body and four requests have
# landed on $D/body since, so the request below is what proves the server has the file.
sheet=$(grep -ho '/assets/index-[A-Za-z0-9_-]*[.]css' "$DIST/index.html" | head -1)
if [ -n "$sheet" ]; then
  req 200 "GET $sheet (the built stylesheet)" GET "$sheet" "Host: $NAME:$PORT"
  css=$(cat "$D/body")
  has "  ...carries the tokens every shadcn component paints with" '--background:' "$css"
  has "  ...in both directions of the theme" '.dark' "$css"
  # The deleted design system must not be reachable from the artefact either: a leftover token
  # or class here means a file in `public/` or an @theme block that nobody can see being used.
  hasnt "  ...and nothing left of the removed palette" '--sx-' "$css"
  hasnt "  ...or of the removed rail classes" '.op-rail' "$css"
else
  skip "GET the built stylesheet" "no /assets/index-*.css reference in the document"
fi
loc=$(location "$NAME:$PORT" -H "Origin: $NAMEORIGIN")
has "Discord sign-in bounces to /login instead of guessing an origin" 'origin-unset' "$loc"
# Relative, and slashed: `/login/` is the route that is prerendered, and a root-level handler
# that bounces to the unslashed form hands the browser to the static layer, which is how a
# redirect turned into a 404 once already. `login_redirect()` in oauth.rs is the only place the
# target is built, so this is the one line that has to keep agreeing with it.
check "  ...with a relative Location, not a hardcoded host" "$loc" "/login/?error=origin-unset"
hasnt "  ...and no redirect_uri was built from a name nobody configured" 'redirect_uri=' "$loc"
stop

printf '\n== the same install reached by IP (the host fallback applies)\n'
start ""
loc=$(location "127.0.0.1:$PORT" -H "Origin: $IPORIGIN")
case "$loc" in
  https://discord.com/oauth2/authorize*) ok "redirect goes to Discord" ;;
  *) no "redirect goes to Discord" "$loc" ;;
esac
has "redirect_uri is the IP the browser dialled" "redirect_uri=http%3A%2F%2F127.0.0.1%3A$PORT%2Fauth%2Fcallback" "$loc"
has "  ...with the scope plus-encoded, as a working link is" 'scope=identify+guilds' "$loc"
hasnt "  ...and no PKCE challenge (a confidential client does not need one)" 'code_challenge' "$loc"
hasnt "  ...and no prompt=none, which fails a first-time consent" 'prompt=none' "$loc"
statev=$(printf '%s' "$loc" | sed -n 's/.*state=\([^&]*\).*/\1/p')
check "  ...with a state of 43 url-safe characters" "$(printf '%s' "$statev" | wc -c | tr -d ' ')" "43"
case "$statev" in
  *[!A-Za-z0-9_-]*) no "  ...and the state is url-safe, not percent-encoded" "$statev" ;;
  *) ok "  ...and the state is url-safe, not percent-encoded" ;;
esac
hasnt "redirect_uri never says localhost when the browser dialled an IP" 'localhost' "$loc"
# The gate that replaces the credential: the pairing route is gone from the router, not
# hidden behind a page. A 404 here is the assertion — a 405 would mean the path is still
# mounted for another verb, and a 401 would mean it is mounted and simply wants a code.
code=$($CURL -o /dev/null -w '%{http_code}' --max-time 10 -X POST -H "Host: 127.0.0.1:$PORT" \
  -H "Origin: $IPORIGIN" -H 'content-type: application/json' -d '{"code":"not-a-real-code"}' \
  "$BASE/api/v1/pair")
check "POST /api/v1/pair is not mounted any more" "$code" "404"
# The page is gone from the router table, so the static fallback answers 404 rather than
# serving a shell that would then fail to fetch anything. A 200 here would mean someone
# re-added the route; a 500 would mean the fallback stopped being a fallback.
code=$($CURL -o /dev/null -w '%{http_code}' --max-time 10 -H "Host: 127.0.0.1:$PORT" "$BASE/pair/")
check "GET /pair/ is not a page any more" "$code" "404"
# The legacy callback stays reachable on purpose: an install whose portal entry was written
# before the move must not have to be re-registered to sign in. It forwards the query, because
# the `code` and `state` Discord just sent are the whole request.
code=$($CURL -o /dev/null -w '%{http_code}' --max-time 10 -H "Host: 127.0.0.1:$PORT" \
  "$BASE/api/v1/auth/discord/callback?code=abc&state=def")
# `Redirect::permanent` is a 308 — the method is kept, so a POST to the old path is forwarded
# as a POST rather than silently turned into a GET.
check "the legacy callback path forwards instead of failing" "$code" "308"
onehop=$($CURL -s -o /dev/null -w '%{redirect_url}' --max-time 10 -H "Host: 127.0.0.1:$PORT" \
  "$BASE/api/v1/auth/discord/callback?code=abc&state=def")
check "  ...to the canonical path, query intact" "$onehop" "$BASE/auth/callback?code=abc&state=def"
stop

# `main.py` prints this path in its own guidance note; if the two ever differ, the note
# teaches people to paste a URI that 404s.
if [ -f "$ROOT/main.py" ]; then
  py=$(sed -n 's/^DISCORD_CALLBACK_PATH = "\(.*\)"/\1/p' "$ROOT/main.py")
  check "main.py's callback path is the route the api serves" "$py" "/auth/callback"
if grep -q 'add_parser("pair"' "$ROOT/main.py"; then
    no "main.py offers no pair subcommand" "the launcher still registers add_parser("pair")"
  else
    ok "main.py offers no pair subcommand"
  fi
else
  skip "main.py's callback path" "no main.py next to this script (running from a partial copy?)"
fi

printf '\n== the origin named by the operator (the deploy guide line: AERONA_BASE_URL, plain http)\n'
start "AERONA_BASE_URL=$NAMEORIGIN"
loc=$(location "$NAME:$PORT" -H "Origin: $NAMEORIGIN")
has "OAuth uses the configured name" "redirect_uri=http%3A%2F%2F$NAME%3A$PORT" "$loc"
hasnt "  ...still no localhost anywhere" 'localhost' "$loc"
code=$($CURL -o /dev/null -w '%{http_code}' --max-time 10 -X POST -H "Host: $NAME:$PORT" \
  -H 'Origin: http://evil.example' -H 'content-type: application/json' -d '{}' "$BASE/api/v1/keys")
check "a hostile Origin is refused before any credential is looked at" "$code" "403"
req 200 "GET / still serves the panel" GET / "Host: $NAME:$PORT"
# The command is gone from the binary too, and the failure is a usage error rather than a
# panic: `python3 main.py` passes unknown words straight through, so the CLI's own message is
# what an operator reads.
out=$(cd "$D" && ./aerona pair 2>&1 | tail -5)
code=$(cd "$D" && ./aerona pair >/dev/null 2>&1; printf '%s' "$?")
check "aerona pair exits non-zero" "$code" "2"
has "  ...and says what the words meant" 'unknown command' "$out"
if [ -f "$D/data/pair-code" ]; then
  no "no pairing code file is written" "data/pair-code exists — something is still minting codes"
else
  ok "no pairing code file is written"
fi
# Cookie attributes on a real session (HttpOnly, SameSite=Lax, no `Secure` over plain http) are
# asserted in `crates/aerona-api/src/auth.rs`, which mints a session through the same
# `create_session` path the login uses — this harness cannot get one, which is the point.
stop

printf '\n== a pinned redirect uri (AERONA_OAUTH_REDIRECT_URI beats the request host)\n'
# Discord matches `redirect_uri` character for character against the application's list, and
# its rejection page never reaches this install. So the value must be the operator's own
# string, byte for byte, on *both* halves of the flow — and a request that arrives by a
# different name must not be able to steer it.
start "AERONA_BASE_URL=$NAMEORIGIN" "AERONA_OAUTH_REDIRECT_URI=http://$NAME:$PORT/auth/callback"
req 200 "GET /api/v1/status with an OAuth application configured" GET /api/v1/status "Host: $NAME:$PORT"
pinned=$(cat "$D/body")
has "  ...reports the uri as pinned" '"redirectUriSource":"pinned"' "$pinned"
has "  ...and echoes the exact string to paste" "\"redirectUri\":\"http://$NAME:$PORT/auth/callback\"" "$pinned"
loc=$(location "127.0.0.1:$PORT" -H "Origin: $NAMEORIGIN")
has "Discord gets the pinned value even when the request came in by IP" "redirect_uri=http%3A%2F%2F$NAME%3A$PORT%2Fauth%2Fcallback" "$loc"
hasnt "  ...and not one built from the IP in the Host header" 'redirect_uri=http%3A%2F%2F127' "$loc"
hasnt "  ...with no PKCE, exactly as the working link is built" 'code_challenge' "$loc"
# The document is prerendered, so this is the static half only: the card is on the page, and
# the sentence that says whether an application is configured arrives on the client from
# /status, which was asserted above.
req 200 "GET /login/ renders the sign-in card" GET /login/ "Host: $NAME:$PORT"

# ------------------------------------------------- the callback, from outside the router
# The last shipped build answered `404 Not Found` here while the handler had in fact run: a
# root-level route that no file matches falls through to the static service, whose fallback
# rewrote the status *after* the redirect headers were set. The Rust suite covers the router;
# this covers the binary with a dashboard build bolted to it, which is the arrangement that
# broke. Three assertions, because the bug was a status with a correct Location on it: the
# code, the target, and the target actually rendering.
req 303 "GET /auth/callback with a state nobody minted bounces" GET "/auth/callback?code=x&state=never-minted" "Host: $NAME:$PORT"
cbloc=$($CURL -D - -o /dev/null -H "Host: $NAME:$PORT" "$BASE/auth/callback?code=x&state=never-minted" | tr -d '\r' | awk 'tolower($1)=="location:"{print substr($0, index($0,":")+2)}')
has "  ...to the page, with the reason as a token" "/login/?error=expired-link" "$cbloc"
hasnt "  ...and a 404 is never what the browser sees" '404' "$cbloc"
req 200 "  ...and the bounce target is a real page" GET "/login/?error=expired-link" "Host: $NAME:$PORT"
stop

printf '\n  %s passed, %s failed, %s skipped\n\n' "$pass" "$fail" "$skipped"
[ "$fail" -eq 0 ] && [ "$skipped" -eq 0 ]
