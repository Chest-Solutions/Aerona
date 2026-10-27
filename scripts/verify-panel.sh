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
  stop || { echo "could not free port $PORT"; exit 2; }
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
  #
  # The pid has to be the *server's*, which `setsid` makes awkward: `$!` named the `setsid`, which
  # exits the moment it forks, so `stop` killed nothing and the next phase found its own port taken.
  # Writing the pid from inside the new session fixes that, and the trailing `&` on the outer
  # subshell is what keeps it a fix rather than a hang: backgrounded, `setsid` is already a group
  # leader, so it forks and the subshell returns. Without that `&` the harness waits on the server
  # forever, which is the one failure mode worse than a leaked process.
  ( cd "$D" && setsid nohup sh -c "./aerona run > srv.log 2>&1 < /dev/null & echo \$! > '$D/pid'; wait" & )
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
  # Then make sure the port is really free, and say so instead of timing out somewhere below it.
  # A harness that fails with "Address in use" reads like the product cannot bind, which is a lie
  # about the wrong thing: this is the script's own leftover.
  i=0
  while [ "$i" -lt 40 ] && command -v ss >/dev/null 2>&1; do
    holder=$(ss -ltnp 2>/dev/null | awk -v pat=":$PORT " '
      index($0, pat) { if (match($0, /pid=[0-9]+/)) { print substr($0, RSTART + 4, RLENGTH - 4); exit } }')
    [ -z "$holder" ] && break
    [ "$i" -eq 20 ] && kill -9 "$holder" 2>/dev/null || true
    i=$((i + 1))
    sleep 0.25
  done
  if [ "$i" -ge 40 ]; then
    printf 'port %s is still held after the server was stopped; the next bind would fail\n' "$PORT"
    return 1
  fi
  return 0
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
# The theme is one class on <html>, decided before the stylesheet paints - by a *file*, because
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
# `Unrecognized feature '…' in 'Permissions-Policy' header` once per page load per name - so the
# retired ones are checked *out*. (b) `Cross-Origin-Opener-Policy` is ignored on a non-secure
# origin, and the browser logs that it is ignoring it; this panel is http by default, so the
# header would be noise and the noise is what the operator reports as "the panel is broken".
# The two features the user saw in *their* console that this repo never sent -
# `attribution-reporting`, `private-aggregation` - come from whatever sits in front of the
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
# A guild page has no file - its URL carries a server id - so the static layer has to own the
# prefix. A 404 on a refresh is a deep link that does not work, and the sign-in return target
# is exactly such a link: this is the one check that keeps `/guild/<id>/<page>/` shareable.
req 200 "GET /guild/<id>/moderation/ serves the app shell" GET /guild/123456789012345678/moderation/ "Host: $NAME:$PORT"
shell=$(cat "$D/body")
has "  ...an html document with the mount point" 'id="root"' "$shell"
has "  ...titled for the panel" 'Aerona dashboard' "$shell"
# The assertion that keeps this round's fix honest. `index.html` is the *prerendered* server list,
# so serving it for a guild URL hands React the wrong document to hydrate: the page recovers by
# discarding the HTML, and the operator's console says `Hydration failed ... #418` on every deep
# link. A shell has no prerendered content in it at all - which is what `main.tsx` looks for to
# decide between mounting and hydrating.
hasnt "  ...and none of the root page's markup (a prerendered body here is a hydration mismatch)" 'id="main"' "$shell"
has "  ...linked theme bootstrap, or a refresh flashes the wrong palette" 'src="/theme-boot.js"' "$shell"
req 200 "GET /access/ (the install-level page)" GET /access/ "Host: $NAME:$PORT"
has "  ...is the access page, prerendered with its own title" 'Access - Aerona dashboard' "$(cat "$D/body")"
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
  # file keeps its name when it is replaced. The old rule - "any 200 that is not HTML" - pinned the
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
# And the failure an operator actually reports: the browser says "Unable to preload CSS for
# http://host:port/assets/x.css" and stops there - a console line with no server-side trace, and no
# hint whether the file is missing, misnamed or refused. Three things make it readable: a real 404 (not
# the SPA shell at 200, which is how an app usually hides a broken build), nothing cacheable about that
# 404 (or a fixed deploy stays shadowed by a cached failure), and one warn line naming the file and the
# root it was looked for in.
miss=$($CURL -o "$D/miss" -w '%{http_code}' -H "Host: $NAME:$PORT" "$BASE/assets/does-not-exist-00000000.css")
check "a missing asset answers 404, not the shell at 200" "$miss" "404"
hasnt "  ...and the miss is nothing a cache may keep" 'immutable' "$(hdrs /assets/does-not-exist-00000000.css)"
has "  ...while the log names the file and the root it was sought in" 'an asset named by the bundle is not in the static root' "$(cat "$D/srv.log")"
# The two contracts a passing build does not guarantee, because in both cases the thing that has to
# be true is a *string* in the shipped bundle: `/guild/<id>/<page>/` must be captured as a param (a
# literal segment matches the URL, puts nothing in `params`, and every sidebar tab re-renders the
# overview - which is the report this repo got twice), and the settings keys must be the model's
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
# The brand is one art file and a generator - `artwork/aerona.png` through `scripts/make-icons.py`
# into `public/` - and both of its failure modes are silent: a document still pointing at the old
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
# Any name is fine: with the build told to keep css together the bundle is `style-<hash>.css` rather
# than `index-<hash>.css`, and the contract is "the document links a stylesheet the server has", not
# the letter of one vite chunk-naming choice.
sheet=$(grep -ho '/assets/[A-Za-z0-9._~-]*[.]css' "$DIST/index.html" | head -1)
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
  skip "GET the built stylesheet" "the document links no stylesheet - that is the whole of the panel's paint"
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
# hidden behind a page. A 404 here is the assertion - a 405 would mean the path is still
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

# The three routes this round added. They are mounted *and* private: a 404 here would mean the
# dashboard is calling something the binary does not have - which is how an earlier round shipped a
# panel whose Save button posted to a path that existed only in the source.
for one in "PUT /api/v1/guilds/1/identity/banner" "DELETE /api/v1/guilds/1/identity/banner" "PUT /api/v1/guilds/1/identity/deco"; do
  m=${one%% *}; path=${one#* }
  req 401 "$m $path answers 401 with no session" "$m" "$path" "Host: $NAME:$PORT"
done

# Round 14's three routes, over HTTP, in the phase that still has a server: mounted, documented, and
# private. A 404 here means the shipped binary and the shipped panel disagree about a path, which is
# the failure a source-only check cannot see.
for p in "/api/v1/manifest" "/api/v1/guilds/1/capes" "/api/v1/guilds/1/capes/mw1/frame/0"; do
  code=$($CURL -o "$D/capebody" -w '%{http_code}' --max-time 10 -H "Host: $NAME:$PORT" "$BASE$p")
  case "$p" in
    */manifest) check "GET $p answers for a cape notifier install" "$code" "200" ;;
    *) check "GET $p needs a session" "$code" "401" ;;
  esac
done
req 200 "the manifest lists the cape routes" GET /api/v1/manifest "Host: $NAME:$PORT"
cape_manifest=$(cat "$D/body")
has "  ...the list the panel reads" "/guilds/{guild}/capes" "$cape_manifest"

# Round 15's route, over HTTP for the same reason: a transcript that opens inline is the fix for
# "transcript htmls don't work", and the fix is only real if the shipped binary answers on the path the
# shipped panel calls. Both halves are private - a public URL for a moderator's ticket channel is not
# something anyone asked for - and the download variant is a header on the same handler, so a 404 on the
# plain path would mean the inline half was never mounted either.
for p in "/api/v1/guilds/1/tickets/1/transcript" "/api/v1/guilds/1/tickets/1/transcript?download=1"; do
  code=$($CURL -o "$D/tx" -w '%{http_code}' --max-time 10 -H "Host: $NAME:$PORT" "$BASE$p")
  check "GET $p needs a session, and exists" "$code" "401"
done
req 200 "the manifest lists the transcript route" GET /api/v1/manifest "Host: $NAME:$PORT"
has "  ...as the path the panel builds" "/guilds/{guild}/tickets/{id}/transcript" "$(cat "$D/body")"
has "  ...the ask that makes the worker look again" "/guilds/{guild}/capes/check" "$cape_manifest"
has "  ...and the render, served by the api from what the worker stored" "capes/{slug}/frame/{frame}" "$cape_manifest"
# C.2 - what the manifest promises, and what the cache headers promise about the files it names.
# (These two lines were the first thing the panel's `no such endpoint` fix needed a guard for, and
# the caching half is the one an operator only notices as a stale banner a week later.)
has "the manifest lists the routes the panel calls" '"/guilds/{guild}/identity/banner"' "$cape_manifest"
# `main.py check` is what an operator runs when the site looks half-broken, so the question "does
# every asset the bundle names actually exist on disk" has to be part of it: a stylesheet that 404s
# leaves a console line in the visitor's browser and nothing at all in the server's.
has "the launcher checks the assets the dashboard html names" "referenced_assets" "$(cat "$ROOT/main.py")"
has "  ...and says which one is missing" "the html names are MISSING" "$(cat "$ROOT/main.py")"
hasnt "  ...nothing in the manifest-era folders promises a year for an unhashed name" 'immutable' "$(hdrs /assets/brand/logo.png)"
hasnt "  ...and the api manifest is not what the page loads" '/api/v1/manifest' "$(cat "$ROOT/dashboard/dist/index.html")"
# The pin in `static_hit` comes from `is_fingerprinted`, which must be narrow in both directions: a
# name in /assets whose suffix is not a digest still gets revalidated, or one refactor that moves an
# editable file under /assets makes it un-refreshable for a year.
hasnt "a dated but unhashed name under /assets is not pinned either" 'immutable' "$(hdrs /assets/brand/panels-20240101.js)"
# The other direction, on a name from this very build. A rule that is too wide breaks a week later;
# a rule that is too narrow breaks on the next commit, because a hash is random: vite's digest is
# url-safe base64, so `_` and `-` land in it as often as letters do, and an alphanumeric-only test
# silently unpinned `/assets/index-B0IWrOJ_.js` while every test name in the file still passed.
assets=$(cd "$DIST/assets" 2>/dev/null && ls | grep -E -- '-[A-Za-z0-9_-]{8,}\.(js|css)$')
# Prefer a name whose digest itself holds a `_` or a second `-`: that is the case a rule too narrow
# drops, and taking the alphabetically first asset would keep the check passing on a build whose hashes
# happened to come out alphanumeric.
hashed=$(printf '%s\n' "$assets" | grep -E -- '_[A-Za-z0-9_-]*\.[a-z]+$|-[A-Za-z0-9_-]*-[A-Za-z0-9_-]*\.[a-z]+$' | head -1)
[ -n "$hashed" ] || hashed=$(printf '%s\n' "$assets" | head -1)
if [ -n "$hashed" ]; then
  has "  ...while this build's hashed asset does promise a year" 'immutable' "$(hdrs "/assets/$hashed")"
else
  skip "  ...while this build's hashed asset does promise a year" "no hashed js/css in $DIST/assets"
fi
# The legacy callback stays reachable on purpose: an install whose portal entry was written
# before the move must not have to be re-registered to sign in. It forwards the query, because
# the `code` and `state` Discord just sent are the whole request.
code=$($CURL -o /dev/null -w '%{http_code}' --max-time 10 -H "Host: 127.0.0.1:$PORT" \
  "$BASE/api/v1/auth/discord/callback?code=abc&state=def")
# `Redirect::permanent` is a 308 - the method is kept, so a POST to the old path is forwarded
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
  no "no pairing code file is written" "data/pair-code exists - something is still minting codes"
else
  ok "no pairing code file is written"
fi
# Cookie attributes on a real session (HttpOnly, SameSite=Lax, no `Secure` over plain http) are
# asserted in `crates/aerona-api/src/auth.rs`, which mints a session through the same
# `create_session` path the login uses - this harness cannot get one, which is the point.
stop

printf '\n== a pinned redirect uri (AERONA_OAUTH_REDIRECT_URI beats the request host)\n'
# Discord matches `redirect_uri` character for character against the application's list, and
# its rejection page never reaches this install. So the value must be the operator's own
# string, byte for byte, on *both* halves of the flow - and a request that arrives by a
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

# ------------------------------------------------- what the panel must and must not contain
# These are grep assertions rather than a rendered-page check because they are about code the
# browser cannot see: which endpoint a page calls, whether a switch drives anything, and whether
# two crates still disagree about how a transcript is built. Every one of them was true of a build
# this project shipped while a user could not get the feature to work, which is the failure mode
# worth scripting.
src() { cat "$ROOT/dashboard/src/$1" 2>/dev/null; }
api="$ROOT/crates/aerona-api/src/routes/guild.rs"
logger="$ROOT/crates/aerona-bot/src/logger.rs"

dashes=$(grep -rho '—' "$ROOT/dashboard/src/pages" "$ROOT/dashboard/src/lib" "$ROOT/dashboard/src/components" 2>/dev/null | wc -l | tr -d ' ')
has "no em dash anywhere in the panel copy" "0" "$dashes"

hasnt "the logging page has no master switch" "key: 'enabled'" "$(src pages/guild/Logs.tsx)"
has "the logging page reads the log the bot writes" "/log?limit=" "$(src lib/api.ts)"
has "a picker can be refreshed on demand" "picker-refresh" "$(src lib/api.ts)"
has "channels are shown with the # they have in Discord" '`#${o.name}`' "$(src lib/settings-form.tsx)"
has "roles are shown in their own colour" "iconUrl" "$(src lib/settings-form.tsx)"
has "a role a human cannot be given is not offered" "assignable !== false" "$(src lib/settings-form.tsx)"
has "a sticky is written here, text and embed and all" "embed_title" "$(src pages/guild/Stickies.tsx)"
has "  ...in the channel it will live in, by name" "Refresh channels" "$(src pages/guild/Stickies.tsx)"
hasnt "a sticky does not need a Discord message to exist" "messageChoices" "$(src pages/guild/Stickies.tsx)"
hasnt "a sticky never asks for a typed id" "label in the list" "$(src pages/guild/Stickies.tsx)"
has "the write that posts a sticky at once" "sticky.post" "$(cat "$api")"
has "  ...and a note keeps the lines it was written with" "opt_block_field" "$(cat "$api")"
hasnt "the avatar stage does not invent a key" "\"avatarHash\"" "$(cat "$api")"
has "  ...it writes the field the model has" "\"avatar_hash\"" "$(cat "$api")"
has "a ticket type is a button, so adding one queues it" "panel.post" "$(cat "$api")"
has "  ...and removing one redraws the board" "panel.redraw" "$(cat "$api")"
has "the types are edited on the page that shows the board" "Ticket types" "$(src pages/guild/Tickets.tsx)"
has "  ...one row per button, with its colour" "t.color" "$(src pages/guild/Tickets.tsx)"
has "  ...and the links under the card" "Link buttons" "$(src pages/guild/Tickets.tsx)"
has "a log row shows what a thing was" "'Before', 'After'" "$(src pages/guild/Logs.tsx)"
has "  ...and what it is now, in the card the channel gets" "fn quote" "$(cat "$logger")"
has "the rail wears the server's own icon" "current={{ id, name, icon, botPresent: present }}" "$(src pages/Guild.tsx)"
hasnt "a field's explanation is not printed under it forever" '<p className="text-[12px]' "$(src lib/settings-form.tsx)"
has "  ...it is a badge that asks to be read" "export function FieldHint" "$(src lib/settings-form.tsx)"
has "the ticket card is editable" "open_title" "$(src pages/guild/Tickets.tsx)"
has "  ...including what the close says" "close_body" "$(src pages/guild/Tickets.tsx)"
has "  ...and its colour" "embed_color" "$(src pages/guild/Tickets.tsx)"
has "html transcripts are offered as a format" "HTML, with images" "$(src pages/guild/Tickets.tsx)"
hasnt "moderation has no switch that drives nothing" "dm_reason" "$(src pages/guild/Moderation.tsx)"

# one renderer, in one crate. The API used to carry its own copy of the transcript builder with a
# different header, so the panel and the bot disagreed about what a transcript said.
has "core builds transcripts" "pub fn render" "$(cat "$ROOT/crates/aerona-core/src/transcript.rs" 2>/dev/null)"
hasnt "the API does not build them a second time" "fn render_transcript" "$(cat "$ROOT/crates/aerona-api/src/routes/guild.rs" 2>/dev/null)"
has "  ...and the bot delegates to core" "aerona_core::transcript::render" "$(cat "$ROOT/crates/aerona-bot/src/events.rs" 2>/dev/null)"
has "the refresh the panel asks for is the one the bot serves" "picker.refresh" "$(cat "$ROOT/crates/aerona-bot/src/outbox.rs" 2>/dev/null)"

# the switch list on the logging page and the list the logger emits must be the same set, or a
# toggle exists for an event that can never happen
panel_events=$(grep -o "^  \['[a-z_]*'," "$ROOT/dashboard/src/pages/guild/Logs.tsx" | tr -d " '[" | sed 's/,$//' | sort | tr '\n' ' ')
core_events=$(sed -n '/pub const LOG_EVENTS/,/];/p' "$ROOT/crates/aerona-core/src/models.rs" | grep -o '"[a-z_]*"' | tr -d '"' | grep -v '^LOG_EVENTS$' | sort | tr '\n' ' ')
if [ "$panel_events" = "$core_events" ]; then
  ok "the logging page offers exactly the events the logger emits"
else
  no "the logging page offers exactly the events the logger emits" "panel: $panel_events | core: $core_events"
fi

# ---------------------------------------------------------------- round 13, by report
# Five bugs and features from one user's list. Each line below is the smallest thing that would have
# told us the report was still true: the switch that should not exist, the field that should, the
# colour that should reach Discord, and the lookup an empty log row came from.
hasnt "the sticky page has no repost switch any more" "key: 'repost'" "$(src pages/guild/Stickies.tsx)"
has "the sticky page says the copy is replaced, not edited" "sends a fresh copy and" "$(src pages/guild/Stickies.tsx)"
has "the API retires an old document's key instead of refusing it" "RETIRED_KEYS" "$(cat "$ROOT/crates/aerona-api/src/routes/mod.rs")"
has "the tickets page names where the board goes" "board_channel_id" "$(src pages/guild/Tickets.tsx)"
has "  ...and saving queues a redraw of it" '"panel.redraw"' "$(cat "$ROOT/crates/aerona-api/src/routes/system.rs")"
has "  ...while a type with no channel inherits it" "board_channel_id" "$(cat "$ROOT/crates/aerona-api/src/routes/guild.rs")"
has "a colour on its own makes a card" "self.embed_color.is_some()" "$(cat "$ROOT/crates/aerona-core/src/models.rs")"
has "the preview is Discord's own renderer, not a drawing of one" "@discord-message-components/react" "$(src pages/guild/shared.tsx)"
has "  ...with its stylesheet, or the preview is unstyled markup" "react/styles" "$(src pages/guild/shared.tsx)"
has "  ...and the props this panel uses are declared, not assumed" "declare module '@discord-message-components/react'" "$(cat "$ROOT/dashboard/src/types/discord-message-components.d.ts" 2>/dev/null)"
has "the stylesheet the preview needs is in the built assets" 'discord-messages' "$(cat "$ROOT"/dashboard/dist/assets/*.css 2>/dev/null | head -c 400000)"
# How the panel is *loaded*, which is the part that broke on somebody's install. With the default
# `cssCodeSplit`, every lazily imported chunk gets its own `assets/<chunk>-<hash>.css` and the router
# fetches it through a preload helper at route-change time; one lost 13 KB file then costs a guild page
# its styling, with a single console line as the evidence. One stylesheet, linked by the document, is
# fetched by the browser's own loader and cannot half-fail.
css_count=$(ls "$DIST/assets"/*.css 2>/dev/null | wc -l | tr -d ' ')
check "the build emits exactly one stylesheet" "$css_count" "1"
css_refs=$(grep -lEo 'assets/[A-Za-z0-9._~-]+\.css' "$DIST/assets"/*.js 2>/dev/null | wc -l | tr -d ' ')
check "  ...and no chunk reaches for another one" "$css_refs" "0"
has "  ...the document links it" 'rel="stylesheet"' "$(cat "$DIST/index.html")"
has "  ...and so does the app shell for deep links" 'rel="stylesheet"' "$(cat "$DIST/shell.html")"
# Counted as its own statement rather than searched as a substring: commenting the option out leaves
# the text `cssCodeSplit: false` in the file inside a `//`, which is how the first version of this
# check stayed green against a build that had stopped doing what it claims.
css_on=$(grep -cE '^[[:space:]]*cssCodeSplit: false[[:space:]]*,?$' "$ROOT/dashboard/vite.config.ts")
check "  ...because the build says so, on a line that is not a comment" "$css_on" "1"
has "the profile page can write a banner" "identity/banner" "$(src lib/api.ts)"
has "  ...and ask for a decoration by id" "identity/deco" "$(src lib/api.ts)"
has "the apply sends account fields to the account route" "Route::UserMe" "$(cat "$ROOT/crates/aerona-bot/src/outbox.rs")"
has "  ...and guild ones to the member route" "Route::GuildMemberMe" "$(cat "$ROOT/crates/aerona-bot/src/outbox.rs")"
hasnt "no card is nailed to blurple in the worker" ".color(0x58_65_F2)" "$(cat "$ROOT/crates/aerona-bot/src/outbox.rs")"
has "a reply card wears the colour the tickets settings chose" "embed_color" "$(cat "$ROOT/crates/aerona-bot/src/outbox.rs")"
has "an event with no guild in it still finds one" "payload.or(from_channel)" "$(cat "$ROOT/crates/aerona-bot/src/events.rs")"
has "a delete quotes what the bot kept when the message arrived" "recalled_message" "$(cat "$ROOT/crates/aerona-bot/src/events.rs")"
has "  ...and an edit is logged from the text it was given" "fn edit_entry" "$(cat "$ROOT/crates/aerona-bot/src/events.rs")"
has "the picker reports the decorations this server wears" "decoration_rows" "$(cat "$ROOT/crates/aerona-bot/src/tasks.rs")"

# ---------------------------------------------------------------- round 14, the cape notifier
# A feature with an outside source and a picture of its own, so the checks are about the two ways it
# could quietly become nothing: a route that is not there (asserted over HTTP, in the phase above),
# and a fetch that happens in the wrong process (asserted here, by the file it must not appear in).
CAPE_CORE="$ROOT/crates/aerona-core/src/cape.rs"
CAPE_DB="$ROOT/crates/aerona-core/src/db/capes.rs"
CAPE_BOT="$ROOT/crates/aerona-bot/src/capes.rs"
CAPE_API="$ROOT/crates/aerona-api/src/routes/guild.rs"
CAPE_BOT_BODY=$(cat "$ROOT/crates/aerona-bot/src/capes.rs")


# The whole split of this product in one `hasnt`: the api writes intent and never reaches out. A
# settings page that fetched minecraft.wiki would be a fetch tool on the public port, and it would
# fail on the wiki's outages rather than on the setting's.
hasnt "the api never fetches a cape itself" "reqwest" "$(cat "$CAPE_API")"
has "  ...the panel asks, in words" "cape.check" "$(cat "$CAPE_API")"
has "  ...and the picture url is the server's, not one the page guesses" "frameUrl" "$(cat "$CAPE_API")"
has "  ...served from the stored bytes" "cape_render" "$(cat "$CAPE_API")"

has "one poll reads the whole category, not the ten the api defaults to" "gcmlimit=500" "$(cat "$CAPE_CORE")"
has "  ...and the file list is a second call, because the generator drops prop=images" "prop=images" "$(cat "$CAPE_CORE")"
has "the source is the wiki, because Mojang publishes no cape list" "Mojang does not publish a cape list" "$(cat "$CAPE_CORE")"
# How the artwork is found, which is the part a fixture cannot answer for: measured on the live
# category, `prop=images` lists nothing for 56 of 60 cape pages, so the names the wiki's own naming
# implies are asked about directly, in chunks under its fifty-title cap, and a MediaWiki error object
# is treated as an error rather than as "no capes".
has "the artwork is asked for by name, because a page usually lists no images" "pub fn wanted_files" "$(cat "$CAPE_CORE")"
has "  ...and what a page does list is filtered to that cape's files" "pub fn about_page" "$(cat "$CAPE_CORE")"
has "  ...in chunks, because past fifty titles the api answers an error" "names.chunks(40)" "$CAPE_BOT_BODY"
has "  ...and an error object is never read as an empty answer" "the wiki said" "$(cat "$CAPE_BOT")"
has "there is a live check of the source, kept off by default" "needs the network" "$(cat "$ROOT/crates/aerona-bot/tests/live_cape_wire.rs")"
has "the render is drawn here, from the flat texture" "pub fn render_frames" "$(cat "$CAPE_CORE")"
has "  ...at four angles, one of which is the hero" "pub const YAWS" "$(cat "$CAPE_CORE")"
has "the frame that reads as cloth is the one the message carries" "pub const HERO_FRAME" "$(cat "$CAPE_CORE")"

has "a cape is announced once per server, ever" "ON CONFLICT (guild_id, slug) DO NOTHING" "$(cat "$CAPE_DB")"
has "  ...and a first look records everything it finds in silence" "silent" "$(cat "$CAPE_DB")"
has "  ...while a page edited later is refreshed without a second ping" "pub fn refresh_cape" "$(cat "$CAPE_DB")"
has "the trickle is capped, so sixty new capes are sixty minutes of news and not one burst" "limit.clamp(1, 10)" "$(cat "$CAPE_DB")"
has "asking for a check is a stamp, not a request" "pub fn force_cape_check" "$(cat "$CAPE_DB")"

has "the notifier runs on the slow tick, per guild on its own interval" "capes::tick" "$(cat "$ROOT/crates/aerona-bot/src/tasks.rs")"
has "the embed points at the file the same message uploads" 'attachment://{filename}' "$(cat "$CAPE_BOT")"
has "  ...with the wiki's own render when there is no flat texture" "is_portrait_render" "$(cat "$CAPE_BOT")"
has "  ...and no announcement is ever lost to a missing picture" "announcing without a picture" "$(cat "$CAPE_BOT")"
has "a channel that keeps refusing is counted, then left alone" "note_cape_attempt" "$(cat "$CAPE_BOT")"
has "  ...and the count is said out loud on the panel" "not posted after" "$(cat "$CAPE_CORE")"

# ---------------------------------------------------------------- round 15, by report
# Tickets that delete themselves, transcripts that open, a delete that says what is missing, and a cape
# render that is a drape. Each line is the smallest thing that would have told us the report was still
# true.

## The transcript is a document, not a link collection.
has "kept pictures are inside the transcript" 'data:' "$(cat "$ROOT/crates/aerona-core/src/transcript.rs")"
has "  ...and the page's own CSP lets them be" 'img-src https: data:' "$(cat "$ROOT/crates/aerona-core/src/transcript.rs")"
has "  ...while the original url stays the link around the picture" 'let link = escape(&url)' "$(cat "$ROOT/crates/aerona-core/src/transcript.rs")"
has "  ...and the footer counts what is in there" 'fn footer_note' "$(cat "$ROOT/crates/aerona-core/src/transcript.rs")"
# Inline unless asked otherwise, on one handler: two routes for one document is how the two halves
# drift, and `private, no-store` is what keeps a ticket transcript out of a shared browser's history.
TX="$(cat "$ROOT/crates/aerona-api/src/routes/guild.rs")"
has "a transcript opens inline unless it is asked to be saved" 'fn transcript_file' "$TX"
has "  ...with the disposition switched by one query key" 'download' "$TX"
has "  ...and never cached, because the document is the current state of a live channel" 'no-store' "$TX"
hasnt "no transcript route reads a Discord url directly" 'cdn.discordapp.com' "$TX"

## Files: what is kept, and for how long.
TICK="$(cat "$ROOT/crates/aerona-core/src/db/tickets.rs")"
has "a close schedules the channel's deletion" 'schedule_ticket_delete' "$(cat "$ROOT/crates/aerona-bot/src/events.rs")"
has "  ...and a reopen un-schedules it" 'schedule_ticket_delete(ticket.id, None)' "$(cat "$ROOT/crates/aerona-bot/src/events.rs")"
has "  ...the deadline is a queue, not a retry loop" 'channel_deleted_at IS NULL' "$TICK"
has "  ...so the janitor needs an idempotent way to record the fact" 'pub fn mark_ticket_channel_deleted' "$TICK"
has "the deletion is executed on the slow tick, not in the close path" 'delete_due_ticket_channels' "$(cat "$ROOT/crates/aerona-bot/src/tasks.rs")"
has "  ...a channel that is already gone counts as deleted, not as failure" 'fn channel_gone' "$(cat "$ROOT/crates/aerona-bot/src/tasks.rs")"
has "  ...and a refusal is retried on a backoff rather than every minute" 'SLOW_SECS' "$(cat "$ROOT/crates/aerona-bot/src/tasks.rs")"
has "the caps live in the store, so a best-effort caller may skip but cannot overflow a document" 'TicketConfig::FILES_MAX' "$TICK"
has "  ...and kept files are addressed by (seq, idx), the way the transcript finds them" 'ON CONFLICT(ticket_id,seq,idx)' "$TICK"
has "pruning removes the copy and leaves the row" 'pub fn prune_ticket_files' "$TICK"
has "the panel has all three knobs" 'delete_after_minutes' "$(src pages/guild/Tickets.tsx)"
has "  ...retention included" 'keep_ticket_files_days' "$(src pages/guild/Tickets.tsx)"

## Deleted messages, which is the complaint that started this round.
EV="$(cat "$ROOT/crates/aerona-bot/src/events.rs")"
has "a delete says why its text is missing rather than claiming it was kept" 'the text was kept from when it arrived' "$EV"
has "  ...naming the switch when the switch is the reason" 'MESSAGE_CONTENT' "$EV"
has "  ...and naming the cache window when the cache is the reason" 'older than this guild' "$EV"
has "the card puts kept text in the body instead of inventing an After field" 'let kept = if e.event' "$(cat "$ROOT/crates/aerona-bot/src/logger.rs")"
has "the cache is written before the bot decides it does not care about a message" 'worth_caching(' "$EV"
has "  ...so a message deleted a minute later still has a text to quote" 'fn worth_caching' "$EV"
has "an intent that is a question is answered in words, not by a typo" 'fn parse_yes_no' "$(cat "$ROOT/crates/aerona-core/src/config.rs")"
has "  ...and a non-answer is refused at boot" 'neither yes nor no' "$(cat "$ROOT/crates/aerona-core/src/config.rs")"
has "  ...while the launcher says what leaving it off costs" 'AERONA_CONTENT_INTENT is' "$(cat "$ROOT/main.py")"
has "the delete and the edit share one lookup, so they cannot disagree about what was cached" 'fn delete_entry' "$EV"
has "  ...and the row is forgotten once the message is gone" 'forget_message' "$EV"

## The cape: an API, and a drape.
has "the source is MediaWiki's own web API, in one documented call per question" 'action=query' "$(cat "$CAPE_CORE")"
has "  ...the category, not a page to parse" 'generator=categorymembers' "$(cat "$CAPE_CORE")"
# The shape of the answer, asserted as an absence: the reason this source is usable at all is that
# MediaWiki answers json. `hasnt` is a substring test, so what is checked is the html a parser would
# need - a doctype and a content type - rather than the names of parsers that are not dependencies.
hasnt "  ...so nothing here has to read html" '<!DOCTYPE' "$(cat "$CAPE_CORE")"
hasnt "  ...and nothing asks the wiki for a page" 'text/html' "$(cat "$CAPE_CORE")"
has "  ...chosen because Mojang publishes no cape list at all" "Mojang does not publish a cape list" "$(cat "$CAPE_CORE")"
has "  ...and the texture names are asked about directly, because the pages list nothing" 'pub fn wanted_files' "$(cat "$CAPE_CORE")"
has "the sheet is the unwrap of a 10x16x1 box, not a rectangle to stretch" 'pub struct Sheet' "$(cat "$CAPE_CORE")"
has "  ...so a texture that is not that shape is refused instead of smeared" 'is not a cape sheet' "$(cat "$CAPE_CORE")"
has "  ...with the busy face chosen as the one the camera sees" 'pub fn design_face' "$(cat "$CAPE_CORE")"
has "  ...lit from the 3D geometry rather than the projected one" 'fn face_normal' "$(cat "$CAPE_CORE")"
has "the frames, the hero and the flat copy are the renderer's constants" 'pub const FRAMES' "$(cat "$CAPE_CORE")"
has "  ...and the model re-exports them, so a frame cannot go missing from the panel" 'crate::cape::FRAMES' "$(cat "$ROOT/crates/aerona-core/src/models.rs")"
has "the notifier asks once per cape, not twice" 'async fn ask_about_files' "$CAPE_BOT_BODY"

printf '\n  %s passed, %s failed, %s skipped\n\n' "$pass" "$fail" "$skipped"
[ "$fail" -eq 0 ] && [ "$skipped" -eq 0 ]
