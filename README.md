# Aerona

> [!CAUTION]
> This project is currently unstable and will not be maintained (By me atleast)
> Use this with caution

A self-hosted Discord moderation and community platform: one SQLite file, two Rust
binaries, and one React app which is the operator dashboard - the whole of it. There is no
marketing site and no front page: `http://host:25154/` *is* the panel, and the only way in
is a Discord sign-in for an account that administers a server. Nothing about Aerona phones
home - the only outbound connections are Discord's API and, if you switch the AI text filter
on, `lab.sightengine.com`.

Everything here runs on a 2 vCPU / 2 GB box. That is a design constraint, not a
benchmark claim: no Redis, no Postgres, no worker pool, no telemetry.

## Status

| Area | State |
| --- | --- |
| `aerona-core` - config, database, models, text pipeline, security, rate limiting, transcripts, the Minecraft cape source and renderer | complete, 187 tests |
| `aerona-api` - `/api/v1`, sessions, API keys, audit, static dashboard | complete, 75 tests |
| `aerona-bot` - gateway worker: moderation, tickets, stickies, anti-raid, anti-spam and link guard, AI filter, welcomer and autoroles, cape notifier, commands | complete, 78 tests, of which 8 are a stress pass over the shipped decisions (`src/stress.rs`); **not yet run against a live guild** |
| `aerona` - the one-process binary: dashboard + API + worker, with the worker retried internally | complete, 21 tests (19 unit, 2 that run the real executable) |
| `dashboard/` - the operator app and nothing else: 4 prerendered routes, an app shell for `/guild/<id>/` deep links, and one page per server inside it, all `noindex` | complete, built on shadcn/ui + Tailwind + Framer Motion and nothing else; `tsc`, the prerender audit and the overlay audit clean, light and dark both follow the OS |
| `main.py` - launcher (get a binary, then run it) | complete, `run` (default) / `build` / `check` / `doctor` / `status` / `key` / `healthcheck` / `reset`, 37 launcher tests |

Verified in this environment: `cargo fmt --check`, `cargo clippy --workspace
--all-targets -- -D warnings`, `cargo test --workspace` (361 tests), `python3
-m unittest discover tests` for the launcher, the dashboard build plus prerender and
audit, and an end-to-end boot of `python3 main.py` - sign-in,
bearer auth, security headers, the static-serve policy, graceful SIGTERM drain, and the
worker starting, reaching Discord's gateway, and being restarted by the supervisor with
backoff when the token is not valid.

Not verified here, by necessity: what the bot does inside a real server. Every handler is
unit-tested for its pure logic (permission resolution, window maths, verdict filtering,
transcript rendering, argument parsing) and compiles against serenity 0.12.5, but
"the button opened a channel and the transcript landed in the log channel" needs a guild.

## Layout

```
Cargo.toml            workspace: core, api, bot, aerona (the unified binary)
crates/aerona-core/   config.rs db/ (schema + per-feature queries) error.rs identity.rs
                      models.rs ratelimit.rs security.rs text.rs time.rs
crates/aerona-api/    app.rs (routes, gate, static policy) auth.rs bootstrap.rs oauth.rs
                      respond.rs server.rs (the lifecycle) routes/{system,guild,account}.rs
crates/aerona-bot/    state.rs gate.rs events.rs commands.rs outbox.rs tasks.rs
                      worker.rs (the gateway connection) main.rs
crates/aerona/        cli.rs stop.rs serve.rs main.rs - one process, both halves
dashboard/            React + Vite + Tailwind, dark-first design system; prerendered to dashboard/dist
bin/                  prebuilt static binaries, `bin/aerona-<platform>` (linux-x86_64 ships)
main.py               launcher: finds or builds a binary, runs it, mints keys, reports
tests/                the launcher's own tests (stdlib unittest, no pytest needed)
.env.example          every setting, with its default and why it exists
```

## Quickstart

```bash
python3 main.py
```

That is the whole instruction. The launcher will:

1. write a first-boot `.env` from `.env.example` (mode `0600`, with a generated
   `AERONA_MASTER_KEY`) if you have none - it never invents a bot token for you, so
   unless you set `AERONA_TOKEN` in `.env` or in the environment it stops and says so;
2. look for a runnable binary, preferring `bin/aerona-<platform>` - the archive ships a
   statically linked `bin/aerona-linux-x86_64` that needs nothing installed, and the
   launcher also puts the execute bit back when a zip or an upload dropped it;
3. if there is no usable one for your platform, install a minimal stable Rust toolchain
   *into `.cache/`* (nothing outside the project directory) and `cargo build --release`,
   then `npm run build` the dashboard if `dashboard/dist` is missing;
4. start the binary and keep it up: output is relayed with secrets redacted, a crashed
   process is restarted with an exponential backoff, and SIGTERM/SIGINT drains it.

To do it by hand instead:

```bash
sh scripts/build-static.sh                            # the artifact in bin/, statically linked
cargo build --release -p aerona                       # or a normal glibc build, for the box you are on
cd dashboard && npm ci && npm run build && cd ..      # optional: without it the API is api-only
AERONA_TOKEN=… AERONA_PORT=8080 ./target/release/aerona
```

## Appearance

The panel is built from shadcn/ui on the `base-nova` preset, Tailwind 4 and Framer Motion, and
nothing else: every control in `dashboard/src/components/ui/` is a registry component, and the
tokens they read are in `dashboard/src/index.css`. Light and dark both exist - the theme follows
the operating system, a toggle in the top bar of every page overrides it, and the choice lives in
that browser's `localStorage` under `aerona.theme`. `dashboard/public/theme-boot.js` applies it
before the stylesheet arrives, which is the whole reason the panel does not flash - and it is a
*file* rather than the inline block it used to be, because every response carries
`Content-Security-Policy: … script-src 'self'`, under which an inline script is not a style choice
but a blocked request and a panel that hydrates against the wrong colours.

There is deliberately no runtime restyle file. A `dist/theme.css` loaded after the bundle was how
a fork could change the look without rebuilding, and it was also a second palette that nothing in
the product could switch back to; the design round that removed it deleted a theme picker, an
accent slider, a radius slider, a vibrancy switch and 124 tokens with it. Change
`index.css` and rebuild instead - the build is `npm run build` in `dashboard/`, and it fails on a
type error rather than shipping a panel that half-works. Three audits ride on the same command, all
of them born from a defect that type-checked happily: the prerender contract (a route that lost its
title, its `#main` mark, a newline in `robots.txt`), the overlay render (a Base UI part outside the
ancestor whose context it reads, which is a minified `#31` and a blank page in production), and the
settings-document audit - every key in every form is checked against the Rust structs in
`crates/aerona-core/src/models.rs`, so the panel cannot offer a field the bot does not read.

Icons come from one set - `lucide-react`, the same glyphs the vendored shadcn primitives expect -
and every guild page and every config card carries its own, which the build enforces: a rail where
nine rows share a shield is a list you have to read rather than scan, and the shadcn primitives
already size `[&_svg:not([class*='size-'])]` to 16 px, so swapping the set cost no per-site sizing.

The mark is one file too: `artwork/aerona.png`, from which `python3 scripts/make-icons.py` derives
every raster the browser and the manifest ask for - favicons at 16, 32 and 48, a 180 px
`apple-touch-icon`, 192 and 512 for `site.webmanifest` plus a 512 px maskable variant, and two ink
versions of the glyph that `components/brand.tsx` swaps with the `dark` class. The generator is a
script rather than nine committed exports so that changing the art is one command, and it doubles
as the check: alpha comes from the art's own ink (its white field is not a shape), and it fails if
an output would land outside its box.

Motion is the preset's own: page transitions are a 200 ms fade and 8 px rise, and
`useReducedMotion` replaces them with nothing at all rather than with a faster version of the
same movement.

Canonical and `og:url` values are baked at build time, so a public install should set the
origin it wants indexed before building:

```bash
cd dashboard && AERONA_SITE_ORIGIN=https://bot.example.com npm run build   # ssr + prerender are in `build`
```

An unset `AERONA_SITE_ORIGIN` leaves `https://aerona.local` in the canonicals, which the
prerender summary says out loud rather than hiding. Everything else about SEO - per-route
title and description, `og:`/`twitter:` with alt text, `article:modified_time`, JSON-LD
`@graph` with `WebSite`/`BreadcrumbList`/`FAQPage`, `robots.txt`, a `sitemap.xml` carrying
`lastmod`, `llms.txt` for agents, and the audit that fails the build on a missing canonical,
a second `<h1>` or a one-line `robots.txt` - is produced by that same prerender step.

On a fresh install the dashboard has no accounts, no passwords and nothing to copy out of a
terminal. Open `http://localhost:8080/` and it asks before it shows you anything: a signed-out
browser is sent to `/login/`, which offers **Sign in with Discord** and nothing else - no server
list, no empty tiles, no error body where a page should be. Press it, approve in Discord, and you
come back to `/`, because the URL you first asked for is carried through the round trip as
`?next=`. A session is minted only when the Discord account manages (Manage Server or
Administrator) a server this bot is actually in, and it is scoped to exactly those servers - the
grant list is a column on the session row. A session that stops being valid (revoked, expired, or
re-minted after `AERONA_MASTER_KEY` changed) is answered with a 401 that clears the cookie and, in
the browser, with a bounce back to that same page - so nobody has to find "clear site data" to get
in. `python3 main.py key create` is how you make the long-lived credential:

```bash
python3 main.py key create --label deploy --scope read --scope moderate --out keys/deploy.txt
```

`--out` writes the plaintext key to a 0600 file, which is why the flag exists rather than
`> key.txt`: the log lines share stdout. A key cannot mint or revoke other keys - `/keys`
is session-only by design - so `key list` and `key revoke` want a dashboard session: run them
with `AERONA_SESSION=<cookie value>` from a signed-in browser, and the reason a key is not
enough there is the same reason it cannot create its siblings.

Other supervisor commands: `check` (config + database + binary discovery, no port bound),
`doctor` (the same with more detail), `build` (cargo + npm), `status` (live process and
queue state from `/api/v1/status`), `healthcheck` (the container probe), `key
list|revoke`, `reset` (archive the database and start clean).

## Configuration

All settings live in the environment or `.env`; see `.env.example` for the annotated list.
The ones that change behaviour rather than numbers:

| Key | Effect |
| --- | --- |
| `AERONA_TOKEN` | the bot token. Required unless `AERONA_BOT=off` |
| `AERONA_PORT` | one port serves `/api/v1` **and** the dashboard. `main.py` falls back to the panel's `PORT`, then `8080` |
| `AERONA_BOT` | `off` runs the dashboard with no gateway worker; it is also the only way the token becomes optional |
| `AERONA_BASE_URL` | the origin allowed for state-changing requests, and the canonical URL in the site's `<link rel="canonical">`. Optional: while it is unset the API accepts the request's own `Host` when that is an IP or `localhost` (see *Requests* in `SECURITY.md`) |
| `AERONA_DB` | SQLite path. Back this file up and you have backed up Aerona |
| `AERONA_MASTER_KEY` | the pepper for session and API-key MACs. Unset, it is derived from the bot token; with no token at all, first boot writes `data/install.key` (0600) |
| `AERONA_CONTENT_INTENT` | `true` to filter text and accept prefix commands (needs the privileged intent on) |
| `AERONA_TRUSTED_PROXY` | `true` only when a proxy you control terminates TLS in front of the API; it is what makes `X-Forwarded-For` authoritative for rate limits and audit |
| `SIGHTENGINE_TOKEN` | `user:secret`; absent means the AI text filter runs on local rules only |
| `AERONA_THREADS` | clamp tokio's worker threads (a container sees the host's CPU count, not its quota) |
| `AERONA_TOOLCHAIN_DIR` | where `main.py` installs Rust when it has to (default `.cache/`) |
| `AERONA_NO_AUTOINSTALL` | `1` = never download a toolchain; use only what is in `bin/` or `target/` |
| `AERONA_BUILD_JOBS` | how many rustc processes a from-source build may start; by default the launcher sizes it from `MemTotal`, because two release rustcs on a 2 GB box is an OOM kill, not a slow build (`auto` = cargo decides) |

`AERONA_TOKEN` and `AERONA_PORT` are the two with no meaningful default *for a bot*, so
`main.py check` refuses to start with either missing - unless `AERONA_BOT=off`, which is
the dashboard-alone case.

## How it works

**One process by default, and it is still two halves.** `aerona` runs the API and the
gateway worker in one process, because a game panel runs one command and a container that
exits looks like a crash. Inside it, the halves keep the same boundary the split binaries
have - the API owns intent (who may do what, what is stored, what is audited) and the
worker owns execution (the gateway, the actions, the reactions to events) - and they still
talk only through the database, never through a socket. `aerona-api` and `aerona-bot` are
that same code as two processes, for a supervisor that wants to restart them
independently.

The failure policy is the part worth knowing: the dashboard decides when the *process*
exits (it cannot bind, or it was told to stop), while the worker is retried in-process with
a 3–60 s backoff, because a Discord outage or a rejected token must not take the site
down. A rejected token logs once per attempt with the words "Discord refused this bot
token". Release builds unwind on panic rather than aborting, which is what lets the
worker's per-event panic isolation mean anything.

Three tables carry every message between the halves:

* `action_outbox` - the API enqueues `moderate.ban`, `ticket.create`, `raid.lockdown`, …
  and the worker claims them, applies them, and records the result. A dashboard request
  therefore returns `202 {id, poll}` instead of hanging on a network call, and a Discord
  outage becomes a backed-up queue instead of a failed request. Rows claimed but not
  finished are requeued after 120 s, which is also how the worker survives a restart
  mid-action.
* `config_epoch` - bumped on every settings write; the worker polls it and refreshes
  cached settings and command registrations, so a config change is live in ~2 minutes
  without the API needing to reach the bot.
* `bot_status` - the worker writes a report per guild every 20 s (latency, shards, cached
  users, open tickets, queue depth, events/min). The dashboard reads it; older than 90 s
  means "the worker is not talking", which is a different state from "the API is down".

**One gate for every request** (`aerona-api/src/app.rs`): identity → metering →
origin/CSRF for anything state-changing → a 30 s handler timeout → the response envelope.
Both auth methods arrive at the same `Principal`, so a script and a browser session get
identical permissions and identical audit rows. API keys are scoped
(`read`, `moderate`, `config`, `tickets`) and can be bound to one guild, in which case
they only work on `/guilds/{that id}/…` and `/me`.

**The text pipeline is shared, not duplicated.** `aerona-core/src/text.rs` compiles a
guild's custom rules, scans text, and turns hits plus provider categories plus strike
count into one verdict. The dashboard's "test a phrase" preview and the worker's live gate
call that same function, so a preview that says "would delete and strike" is the behaviour
you get.

**The cape notifier is the one thing that reaches outside, and only the worker does it.**
Minecraft publishes no cape list - the asset index and the client jar contain no cape
textures, so there is nothing to poll at Mojang - which leaves minecraft.wiki's
`Category:Capes` as the source: one `gcmlimit=500` request per pass for every page's id,
title and `lastrevid`. The files are a second request, and mostly it answers nothing: the wiki names
a cape's files from an infobox template rather than embedding them, so 56 of the 60 pages in the
category list no images at all, and the four that do list their gallery noise (Turtle Cape offers a
disambiguation dot, a minecart and an open barrel before anything about turtles). What gets asked for
instead are the names the wiki's own convention implies - `File:<Cape> (texture) JE.png`,
`File:<Cape> (texture).png`, `File:<Cape>.png` and three more - in chunks under the API's fifty-title
cap, which is where the artwork actually comes from.
"A cape is a cape" is a per-guild fact (`ON CONFLICT (guild_id, slug) DO NOTHING`), and a
first pass records everything it finds in silence, because announcing sixty old capes to a
channel that was just configured is worse than announcing nothing. The picture is chosen
from the flat 64x32 texture when the wiki has one: `aerona-core/src/cape.rs` drapes that
texture over the game's own cape layout and projects it at four yaws, so a message carries
folded cloth rather than a UV sheet; with no texture it falls back to the wiki's portrait
render, and with neither it still announces - "here is the page" beats silence. The panel
never fetches. `POST /guilds/{g}/capes/check` stamps the row and answers in words that the
worker will look within a minute, and the render is served back out of SQLite at
`/guilds/{g}/capes/{slug}/frame/{n}`, so a Discord outage does not lose the announcement and
a channel that keeps refusing gets five tries and then a line on the panel.
`crates/aerona-bot/tests/live_cape_wire.rs` walks that whole path against the live wiki - newest
pages, both requests, the picker, the renderer - and is `#[ignore]`d, so `cargo test` never depends on
somebody else's uptime, while an operator who wants to know whether a quiet notifier is us or them has
one command to run.

**Static serving is a whitelist, not a catch-all.** Only `dashboard/dist` is exposed. A
hidden path segment (`/.git`, `/.env`, `dist/.template.html`) is refused; a miss under an
`Accept: text/html` request returns the prerendered `/404/` page body at status 404;
every other miss gets the JSON error envelope. Hashed assets are `immutable`, HTML is
`no-cache`, API responses are `no-store`, and a miss under `assets/` is a 404 that no cache may keep
plus one warn line naming the file and the root it was looked for in - because the only links to
`/assets/<name>-<hash>.css` are the ones the build wrote, so a miss there is not a visitor's typo, it
is a half-copied deploy, and a browser reports it as a single line nobody can act on.

The panel is deliberately fragile-proof in the same direction: the build is told to keep css together
(`cssCodeSplit: false`), so the whole panel is one `assets/style-<hash>.css` in a `<link>` the document
carries, fetched by the browser's own loader. Splitting it per lazily loaded chunk would mean the
router preloading a stylesheet at route-change time, where one lost 13 KB file costs a guild page all
of its styling and leaves nothing in the server log. `python3 main.py check` reads the html's own asset
list and says which names have no file behind them.

## Intents, and why each is asked for

| Intent | Used for | Privileged |
| --- | --- | --- |
| `GUILDS` | knowing which servers the bot is in, member counts, role names | no |
| `GUILD_MEMBERS` | join/leave events: anti-raid, autoroles, the welcomer (message, embed card, optional DM), kick/ban accounting | yes |
| `GUILD_MESSAGES` | message create/update/delete: the filter, prefix commands, stickies, edit and delete logging | no |
| `MESSAGE_CONTENT` | reading message text at all - the filter and prefix commands are inert without it | yes |
| `GUILD_MODERATION` | ban/unban events, so bans placed outside Aerona are recorded | no |

`GUILD_MEMBERS` and `MESSAGE_CONTENT` must also be toggled in the Discord developer
portal; `aerona check` prints the intent names it asked for (and `main.py check` relays
them), and the worker logs a warning naming the setting when `MESSAGE_CONTENT` is off.
Nothing in Aerona reads message content unless you turn that on.

## Operations

### Running it on a game panel (Pterodactyl, Altare, and friends)

The yolks python images have a python and nothing else, so the panel path is the one the
launcher is built around:

* **Startup command**: `/usr/local/bin/python /home/container/main.py` - no subcommand
  needed, `run` is the default. An install script that calls `main.py build` also works
  and is a no-op when a binary is already in place.
* **Environment**, in the panel's variables rather than a `.env` you have to edit:
  `AERONA_TOKEN`, `AERONA_PORT` (or rely on the panel's own `PORT`), `AERONA_BASE_URL`
  if you have a domain, and `AERONA_BOT=off` if you want the site up before the bot exists.
* **No toolchain required**: `bin/aerona-linux-x86_64` is a static musl binary, so it runs
  on any x86_64 Linux regardless of the image's glibc. If your panel is a different
  architecture, the launcher will install Rust into `.cache/` and build - budget about
  2 GB of disk and ten minutes of CPU, and `rm -rf .cache target` afterwards to hand the
  space back.
* **Browse it over `http://`, not `https://`.** The port the panel allocates is a raw TCP
  forward into the container, and Aerona speaks plain HTTP there - nothing terminates TLS for
  you. Opening `https://<node>:<port>` gets `ERR_SSL_PROTOCOL_ERROR`, because the server
  answered a TLS handshake with a plaintext response. If you want HTTPS, put your own
  proxy in front and set `AERONA_BASE_URL` to that `https://` origin.
* **Set `AERONA_BASE_URL` to exactly what the browser types**, including the port:
  `AERONA_BASE_URL=http://your-node.example:25154`, no trailing slash. While it is unset the
  API accepts its own host only for an IP literal or `localhost`; a *named* host is refused
  there because that is the one case DNS rebinding controls. So on a named panel address every
  write will come back `403 cross-origin request blocked` until you name the origin. Plain HTTP
  also means the session cookie - and every moderation action it authorises - travels
  unencrypted: fine for a throwaway install, not for a server with a moderation history -
  finish the setup behind TLS if you can.
* **`main.py` warns about that, it does not refuse it.** An install on a panel port *is*
  plain http on a public address; a launcher that refused to start there was refusing the
  normal case. It logs the exposure in `notes()` and starts; the session cookie carries no
  `Secure` flag until `AERONA_BASE_URL` says `https`.
* **Discord sign-in needs one exact string, and the panel prints it.** The `redirect_uri`
  sent to Discord is `AERONA_OAUTH_REDIRECT_URI` when you set it; otherwise it is derived -
  from `AERONA_BASE_URL` when that is set, and otherwise from the request's own `Host`, but
  only when that host is provably this box (an IP literal or `localhost`). It is never derived
  from a *name* the install was not configured with: a `Host` header reflected into a
  `redirect_uri` is how an authorization code gets steered to somebody else's server. So
  `/login` shows the exact value this install is about to send, with a copy button, and
  `python3 main.py check` prints the same string.
* **Discord's `Invalid OAuth2 redirect_uri` screen is not an Aerona error, and it is not in
  the logs.** It is rendered by Discord, in your browser, before any request reaches your
  install - which is why nothing here can quote it back at you. It has exactly one meaning:
  the `redirect_uri` in the authorize URL is not, character for character, one of the entries
  under OAuth2 → Redirects in your application. The list matches literally, so all of these
  are different URIs to Discord: `http` vs `https`, `localhost` vs the IP you actually type,
  a missing `:25154`, a trailing `/`, a missing `/callback`. What people miss is the second
  half: the portal needs **Save Changes** after every Add Redirect, and if you reach the
  panel by more than one address, each needs its own entry. To stop guessing:
  1. open `http://<the address in your bar>/login/` and copy the address it shows;
  2. paste it into OAuth2 → Redirects, press Save Changes;
  3. try sign-in again. If you want one value that never moves, pin it in `.env` -
     `AERONA_OAUTH_REDIRECT_URI=http://your-node.example:25154/auth/callback` -
     and the same string is then used verbatim for both `/authorize` and the token exchange,
     even if a proxy rewrites `Host` between the two hops.
* **Healthcheck**: `python3 main.py healthcheck` (exit 0/1), or
  `GET /api/v1/healthz` if the panel can probe a port.
* A container restart is normal and loses nothing: the outbox, the config epochs and the
  worker's last report are all rows in SQLite.

* Liveness: `GET /api/v1/healthz` (no auth, never rate limited) and `python3 main.py
  healthcheck` for the container probe.
* What's happening: `GET /api/v1/status` reports version, uptime, guild *count*, queue
  depth, database size, the feature flags, the OAuth redirect source, and the worker's last
  report per guild. It is
  deliberately unauthenticated - a supervisor has to be able to ask before anyone has signed
  in, and it names no guild, no user
  and no secret. `GET /api/v1/manifest` is the same idea for the API surface: every route,
  its auth requirement and its limits. Everything else, including which guilds exist and
  what is in them, is `session|key` only.
* Retention: an hourly pass prunes audit, AI events, outbox, stats and stale bot-status
  rows to `AERONA_RETENTION_DAYS` (90) and caps the per-guild log cache. The database runs
  with `auto_vacuum = INCREMENTAL` and the pass finishes with `PRAGMA optimize`, so the
  file reclaims pages as it goes; a full `VACUUM` (`Db::vacuum`) is left for a maintenance
  window because it rewrites the whole file.
* Backup: copy `AERONA_DB` while the processes are stopped, or use `sqlite3 ".backup"`.
  The audit table is hash-chained; the dashboard's Audit page recomputes it and says
  whether the chain verifies, so a restored file can be checked before you trust it.
* Limits worth knowing: 512 KiB request bodies, 4 SQLite connections per process, 20
  req/s sustained per principal in burst (1 req/s for reads), 12-hour sessions, and one
  sign-in door: Discord.
* What to ask when it is not working: `python3 main.py doctor` - the config answers, plus
  whether the port can be bound, whether `dashboard/dist` looks like a real prerendered
  build, and what Discord says about the token right now.
* systemd: `ExecStart=/opt/aerona/python3 main.py run` with `WorkingDirectory=/opt/aerona`
  and `Restart=always`; Aerona's own supervisor already does the per-child restarts, so
  keep systemd's backoff coarse.

## Testing

```bash
cargo test --workspace          # 313 tests: 164 core, 66 api, 62 bot, 19 aerona, 2 cli
cargo clippy --workspace --all-targets -- -D warnings
cd dashboard && npm run build   # typecheck → client → ssr → prerender (routes + app shell + audits)
python3 -m unittest discover -s tests -t .   # the launcher: binary discovery, .env, toolchain
```

### Developing the panel

```bash
cd dashboard && npm run dev        # Vite on :5173, bound to 0.0.0.0, /api proxied to :8790
cd dashboard && npm run preview    # the built dist/ over :4173, same origin, no dev server
```

The proxy target is one line in `dashboard/vite.config.ts`. Point that line at the API rather than
pointing the panel at an absolute URL, because the panel has no API base to configure: every
request in `src/lib/api.ts` is relative to the page's own origin, so one `dist/` works behind any
domain and there is nothing to mis-set. It also means a preview environment that cannot reach its
own origin shows the offline and error states by design, instead of guessing at `localhost:8080`
through a URL rewrite - which is what a sandboxed preview is. The server's half of that decision is
`AERONA_BASE_URL` (see Configuration): a deployed panel needs it for the origin check and its
canonical URLs, and the browser needs nothing from it.

`scripts/verify-panel.sh` is the check that reaches the wire. It starts the release binary (or
`target/debug/aerona`) on a spare port against a fresh database, crawls every link the built
dashboard contains, and asserts the deep links, a refresh on each of them, an API round-trip
through a real `Set-Cookie`, the pairing code, and the shell served for a path that is not a route.
It exits 65 when the `dist/` embedded in the binary is not byte-identical to the directory it was
built from, which makes it the check that a shipped zip is the tree it came out of:

```bash
sh scripts/verify-panel.sh /path/to/extracted/aerona-v1.0.0-linux-x86_64
```

Two of the dashboard steps are audits rather than builds, and they are the ones that pay for
themselves. `scripts/prerender.mjs` fails the build on a route that lost its title, its `#main` mark, a
newline in `robots.txt`, or a caption implemented as a group-scoped Base UI primitive (see
`TODO.md` §15: that one produced a minified `Base UI error #31` and a guild page that blanked on
click). `scripts/audit-overlays.mjs` renders every popup component the panel opens, so a part used
outside the ancestor whose context it reads is a build failure rather than a white screen - and it says
in its own output which cases a server render cannot assert, so it cannot quietly stop meaning
anything.

The launcher tests are stdlib-only for the same reason the launcher is: they have to run on
a box where nothing is installed yet.

The API tests run a real loopback server and drive it with a real HTTP client, so the
`gate`, the cookie handling, the CSRF rules and the static policy are exercised as
deployed. The worker's tests cover the parts that are pure - permission resolution, the
raid windows, verdict filtering, argument parsing for both command dialects, transcript
and template rendering - because the rest is a network call to Discord. `src/stress.rs`
drives the decisions those calls are made of at a real server's volume: five thousand
log events through the gate that decides whether a guild wanted them, two thousand
transcript rows through the renderer the panel downloads, the DM policy with the guild's
switch flipped both ways, and a lockdown release reading back exactly what the lock
changed. Sends are the one part a test cannot reach without a gateway, so the suite
asserts the payload each handler built instead.

See `SECURITY.md` for the threat model, and `TODO.md` for what is deliberately not here.
