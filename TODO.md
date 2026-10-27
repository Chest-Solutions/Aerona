# Aerona - Build Checklist

Working checklist for this build. Checked items are done; sub-items list what "done" means.
Legend: `[x]` verified  ·  `[~]` implemented, verification pending  ·  `[ ]` not started

---

## 0. Recon & environment

- [x] Read `DESIGN_GUIDELINES.md` (Aero design system) in full
- [x] Probe toolchain: node 20 + npm 10, python 3.13, **no** rust → install rustup 1.99.1 (stable)
- [x] Confirm crates.io sparse index + static.crates.io reachable (API endpoint 403 → pin versions explicitly)
- [x] Pull exact Sightengine text-moderation contract from docs
  - `POST https://api.sightengine.com/1.0/text/check.json`
  - params: `text`, `mode=rules|ml|username`, `categories=...`, `lang`, `list`
  - responses: `profanity.matches[] {type,intensity,match,start,end}`, `<category>.matches[]`,
    `moderation_classes.{sexual,discriminatory,insulting,violent,toxic,self-harm}` (0..1)
- [x] Resolve design conflict: guidelines are light-mode marketing; user asked for a **dark-mode dashboard**
  → keep Aero's tokens/typography/glass/motion/SEO/a11y rules, invert the palette, keep the "no clutter" rule
- [x] Smoke-build dependency stack - 237 crates resolved (serenity 0.12.5, axum 0.8.9, rusqlite 0.32.1); found and fixed a footgun: `regex` without `unicode-case` silently kills case-insensitive matching

## 1. Project skeleton

- [x] `Cargo.toml` workspace: `crates/aerona-core`, `crates/aerona-bot`, `crates/aerona-api` (+ lean release profile, `panic=abort`, thin LTO)
- [x] `dashboard/` Vite 8 + React 19 + TS strict + Tailwind 4 + Framer Motion 14 (versions as built;
      `npm run build` fails on a type error before bundling anything)
- [x] `main.py` supervisor (the only entrypoint) + `requirements.txt` - no `aerona/` python package:
      the launcher is one stdlib-only file, so a panel's `pip install -r requirements.txt` step is a
      documented no-op rather than a wheel to fetch
- [x] `.env` / `.env.example`, `.gitignore` (`/data`, `*.db*`, `.env` all ignored), `docs/`, `scripts/`
- [x] Size guard: `scripts/release-zip.py`. Budgets (128 MB, 10 000 entries) checked against the
      repo *and* the archive; it refuses instead of warning - on a secret-looking file (`.env`,
      `*.key`, `*.pem`, a `*.db` in the tree), on a missing `bin/aerona-<platform>`, and on a missing
      `dashboard/dist/index.html`, because an archive without the built site is a source dump and the
      whole promise of the archive is that nobody has to build anything. Prints size, md5, a `.md5`
      sibling, per-entry probes and the binary's own `--version`

## 2. `aerona-core` - shared layer

- [x] `config.rs`: strict `.env` loader (`parse_env` pure + 15 unit tests), port/token/SIGHTENGINE_TOKEN validation, secrets redacted everywhere, `AERONA_TOKEN`, `AERONA_PORT`, `SIGHTENGINE_TOKEN` (+ optional overrides)
- [x] Secrets zeroized on drop, never logged, constant-time comparisons - `security.rs:72`
      (`ManuallyDrop::take(..).zeroize()` in `Drop`), `security.rs:139` `ct_eq` used by `api/auth.rs:308`
      (CSRF), `db/settings.rs:518` and `db/security.rs:823` (key/pair hashes), `:576` (bound IP);
      `#[test] secret_redacts_and_zeroizes` covers the redaction path
- [x] `db/`: r2d2 + rusqlite (bundled) - `db/mod.rs:68`, pragmas at `:266-269` (`journal_mode=WAL`,
      `foreign_keys=ON`, `busy_timeout=5000`) and `PRAGMA optimize` on close at `:235`
- [x] `db/schema.rs`: single v1 schema, 29 tables + indexes; cascade delete verified by test: forward-only versioned SQL, `user_version` gated, idempotent
- [x] Schema (all `.db`, no JSON files): `grep -c "CREATE TABLE" crates/aerona-core/src/db/schema.rs`
      → 29 tables - guilds, mod config, warns/strikes, tickets+messages+panels, sticky, anti-raid,
      AI filter + custom rules, autorole, welcome, levels, logging, notes, api_keys, sessions,
      audit_log (hash-chained), stats
- [x] `models.rs`: every config/record type with `#[serde(default)]` partial-patch semantics (76 core tests): serde types shared by bot + api + tests (single source of truth)
- [x] `text.rs`: severity ladder, strictness dial, custom-rule compiler + ReDoS guard, provider→hit mapping, `decide()` with strikes/escalation: severity scoring + intensity/score math for Sightengine verdicts
- [x] `ratelimit.rs`: token bucket + fixed-capacity sliding window ring (tests caught and fixed a ring-index bug and a `retry_after` overstatement): token bucket + sliding window (used by anti-raid and API)
- [x] `identity.rs`: snowflake parse (rejects leading zeros/oversize), age from ID, `AER-XXXXXX` ticket refs, base64url ids: Discord snowflake ↔ timestamp, validation
- [x] `error.rs`: stable error codes, status mapping, client-safe messages (test proves DB paths never leak) / `respond.rs`: uniform API envelope `{data|error,meta}`, safe error messages

## 3. `aerona-bot` - Discord (serenity, cache-light)

Gateway + event pipeline, all state in SQLite:

- [x] Moderation: `warn` (+reason), `warns`, `clearwarn`, `pardon`, `kick`, `ban`, `unban`,
      `timeout`, `softban`, `purge`, mass-scan, auto-strike escalation on warn thresholds
- [x] Tickets: panel messages, buttons (open/close/claim/transcript), per-user channel creation,
      category + support role + limits + auto-close stale, transcript to log channel
- [x] Sticky messages: store + reschedule, batched edits across channels, cooldown to avoid edit spam
- [x] Anti-raid / anti-DDoS: join-rate burst detection, msg flood detection, token-bucket per user/guild,
      new-account + avatar-less heuristics, auto actions (verification level, slowmode, lockdown, mass-ban sweep),
      raid alert + recovery, whitelist
      (also HTTP-side: request/body caps, no amplification endpoints, rate-limited, so *our* API can't be used in a DDoS)
- [x] AI filter (text): message gate before it counts, per-guild config, categories/modes/thresholds,
      severity floor, strikes, per-category actions, fail-safe mode, 60s verdict cache
- [x] "A bunch more": autoroles, welcome/goodbye, logging (msg edit/delete, join/leave, name changes),
      user notes, `!stats` (CPU/RSS/uptime/gateway latency), `!ping`, level/XP with announcement DM,
      reminders-free `!serverinfo`/`!userinfo`, custom `!commands`
- [x] Prefix command framework with per-guild permission checks + slash commands (register at startup)
- [x] Rate-limit-aware action queue (Discord 429 backoff), batched sticky edits, presence
- [x] Graceful shutdown on SIGTERM from supervisor; no panics on payload garbage (`catch_unwind` per event)
- [x] Self-check: memory/CPU sampler writes `stats` rows every 30s (bounded table)

## 4. `aerona-api` - dashboard backend at `/api/v1`
- [x] `lib.rs` - `serve` / `serve_from_config` / `build_router`
- [x] `app.rs` - router, `AppState` (config, db, pools, buckets, backoff), the one `gate`
      layer (request id, metering, origin, CSRF, timeout, response decorator), static
      serving of `dashboard/dist` with per-kind cache headers, an `/api/v1/*` fallback
      that answers with the error envelope instead of HTML, `--check` self-test, stats task
- [x] `auth.rs` - session-cookie and `Bearer`/`X-Aerona-Key` extractors, `Principal`,
      `Authed`, `Guild` (authorization folded into extraction so a new route cannot forget
      it), CSRF compare, origin check, `client_ip` from `ConnectInfo` - the socket peer -
      and `X-Forwarded-For` only when `AERONA_TRUSTED_PROXY=1`
- [x] `respond.rs` - envelope `{"data":…,"meta":{apiVersion,at}}`, error envelope carrying
      `safe_message` only, `JsonBody` (413 by declared length, 400 on bad JSON),
      `ok`/`created`/`bad_request`/`invalid`, `X-Request-Id`
- [x] `routes/system.rs` - `GET /status`, `GET /healthz`, `GET /manifest`, `POST /pair`,
      OAuth start + callback, `POST /logout`, `GET /guilds`, `GET /guilds/{guild}`
- [x] `routes/guild.rs` - guild overview, settings for every feature (typed GET/PATCH +
      history), moderation (stats, warn list/create/pardon/clear, strikes, notes,
      top-warned, the actions outbox), tickets (list/create/get/patch/claim/unclaim/
      message + transcript as JSON or as a text download, panels), stickies CRUD,
      raid events + manual lockdown, AI moderation (events, `test` dry run, rule CRUD),
      level leaderboard
- [x] `routes/account.rs` - `GET /me`, audit list + chain verify, keys list/create/revoke,
      sessions list/revoke/revoke-all, stats snapshot + series, `GET /outbox/{id}`,
      `POST /housekeeping`
- [x] `oauth.rs` - Discord login: PKCE S256 + state TTL, guild fetch, `guilds.read`
      degradation, first-login admin bootstrap via `AERONA_ADMIN_USER`, redirect to the
      SPA with a single-use session token
- [x] `bootstrap.rs` - `PairCode` state machine + `data/pair-code` (0600, deleted on the
      first redeem, `aerona-api --pair` to re-mint), canonical code normalisation
- [x] `main.rs` - `run` / `--check` / `--pair`
- [x] `testutil.rs` + `tests.rs` - 13 end-to-end HTTP tests over a real loopback server
      sharing the state: public surface, the 401 matrix, pairing (normalisation, cookie
      flags, file deleted, code absent from the DB, replay refused, failure audited,
      burst → 429), minting only on a fresh install, CSRF/origin matrix, logout +
      revoke-all, key lifecycle with the guild fence, the bot-presence guard
      (404/409/400/403), settings merge/validate/history, warn → escalation → outbox
      claim/finish, tickets disabled → enabled, AI test + rule CRUD, audit chain verify
      (tamper → 500), maintenance/stats, body cap
- [x] `core/src/text.rs` - rule-carried actions and strikes reach the verdict, so a
      `delete_kick` rule is not silently downgraded to the category default
- [x] `core/src/error.rs` - `PayloadTooLarge` (413, `payload_too_large`)
- [x] `core/src/db/security.rs` - `outbox_status` returns the action and args, so
      `/actions/{id}` can answer "what did I queue?"
- [x] tests green: `cargo test --workspace` (core 138, api 57, no warnings)
- [x] `cargo clippy -p aerona-core -p aerona-api --all-targets` - clean (mechanical lints
      fixed via `cargo clippy --fix`, diff reviewed by hand)
- [x] `main.rs --check` is the self-test `main.py` runs before serving: config summary,
      `PRAGMA integrity_check`, db size, guild count, install id, dashboard built-ness
- [x] no `selfcheck` HTTP route on purpose: everything it would report needs a process
      the request cannot see (Discord gateway, systemd socket). `GET /status` carries
      the live half and `--check` the startup half

## 5. `dashboard/` - dark, minimal, SEO-ready

- [x] Design tokens live *only* in `dashboard/src/index.css` now, dark-first, with a
      `[data-theme='light']` override block; `docs/DESIGN.md` is the contract (§9)
- [x] Primitives: `Rails`, `Band`, `SectionHeader`, `Reveal`, `Button`, glass `Card`, `Field`, `Toggle`,
      `Slider`, `Select`, `Table`, `Modal` (native `<dialog>`: focus trap, Esc and backdrop from the
      platform), `Tabs`, `Stat`, `Toast`, plus `AppearancePanel` for the per-browser look (§9)
- [x] Marketing pages (prerendered + JSON-LD): `/`, `/features/`, `/commands/`, `/security/`, `/docs/`, `/legal/privacy/`, `/legal/terms/` - 7 indexable of 18 routes, the rest `noindex` with no canonical
  **Superseded by §13:** the seven marketing pages are deleted, `/` is the dashboard, and the
  route table is 12 routes with 0 indexable. `robots.txt` says `Disallow: /`.
- [x] App (auth-gated): overview, moderation, tickets, sticky messages, anti-raid, AI filter,
      audit, settings, access (keys + sessions). Autoroles, welcome, logging and commands are
      *features* of the settings document - one editor with ten tabs, not ten pages - because
      they share the validated-write path and the history view
- [x] Real data wiring: typed `api.ts` client with CSRF header, `Vary`-safe fetches, optimistic toggles,
      inline validation errors, loading skeletons, `AbortController` on nav
- [~] Motion: `rise` entrance, scroll `Reveal`, spring nav indicator, `AnimatePresence` on the toast
      stack and the tab indicator,
      count-up stats; `prefers-reduced-motion` respected; no cursor-chasing UI; glass never under a `filter` ancestor
- [x] A11y: landmarks, one `h1`/page (the prerender audit now *fails the build* on a second),
      skip link, `--color-ring` focus-visible token, arrow-key tabs, labelled inputs,
      live regions for toasts, no horizontal overflow at 360/390/768
- [x] SEO: per-route `meta()`, canonical w/ trailing slash, OG/Twitter incl. `og:image:alt` and
      `article:modified_time`, JSON-LD `@graph` w/ BreadcrumbList, sitemap with `lastmod`,
      robots.txt, llms.txt - enforced by the build audit, not by this sentence (§9)
      (Organization, WebSite, WebPage, BreadcrumbList, SoftwareApplication, FAQPage, ItemList),
      `sitemap.xml`, `robots.txt`, `llms.txt`, `site.webmanifest`, favicon, `og-image.png`
- [x] `scripts/prerender.mjs` builds every route to `dist/<path>/index.html` + fails on broken links/anchors,
      missing breadcrumb or invalid JSON-LD
- [x] Self-hosted variable fonts (Inter + JetBrains Mono) w/ metrics-matched fallback + preload, no layout shift

## 6. `main.py` - the python wrapper

- [x] CLI: `run` (the default, so `python3 main.py` alone works), `build`, `check`/`doctor`, `status`,
      `pair`, `healthcheck`, `key create|revoke`, `reset`, `--dev`, `--profile`, `--no-build`, `--once`
- [x] Loads `.env` (the environment wins; `.env.local` overrides), honours a panel's `PORT`, validates port
      range and privilege, token shape, `AERONA_BASE_URL` scheme/trailing slash and data-dir writability,
      and refuses to start on a problem it cannot fix - saying which key, not just "invalid config"
- [x] Finds a runnable binary in this order: `bin/aerona-<platform>` (shipped, static) → `target/{release,debug}`
      → *install a toolchain into `.cache/` and build* → legacy `aerona-api` + `aerona-bot` pair. A `--version`
      probe decides "runnable", the exec bit is restored when a zip dropped it, and `AERONA_NO_AUTOINSTALL=1`
      turns the download off. Auto-builds the dashboard when `dist/index.html` is missing (skips, not fails,
      when npm is absent)
- [x] Supervises whatever it launched (one child for the unified binary, two for the legacy pair) with
      exponential backoff, crash-loop detection, and a SIGTERM→SIGKILL escalation after a grace period; the
      `AERONA_PORT` free-check runs before the child starts, so "port already in use" is named as such
      instead of looking like a crash loop. The binary owns signal handling; `--once` turns restarts off
- [x] Structured single-line stdout logging with timestamps and a level; every relayed line is scrubbed of
      `AERONA_TOKEN` / `AERONA_MASTER_KEY` / `SIGHTENGINE_TOKEN` values and of `aer_live_…` keys
- [x] `.venv`-free (stdlib only) so it runs anywhere; `requirements.txt` lists optional dev tooling

## 7. Verification & delivery

- [x] `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` - green (266 tests: 141 core / 61 api / 43 bot / 21 aerona)
- [x] `python3 -m unittest discover -s tests -t .` - green (32 launcher tests, stdlib only)
- [x] `npm run build` + prerender + SEO/link audit green; `tsc --noEmit` clean
- [x] End-to-end: `python3 main.py` boots, `/api/v1/status` 200, dashboard served, security headers present,
      unauth API 401, rate limit fires, CSRF fires
- [x] Resource check: RSS of the API after warm-up (8 MB) and of the unified process; DB size after a test run
- [x] `README.md` (setup, env, architecture, intents, feature matrix) + `SECURITY.md` (threat model, credential handling, limits stated plainly)
- [x] Zip codebase (excl. `node_modules`/`target`/runtime data) → `aerona.zip`, uploaded to Drive
- [x] Report what was run, what passed, and what could not be verified in this sandbox (no Discord token,
      no real browser, no egress to Discord) - the numbers live in §9 and §10 below and travel with the
      archive as `RELEASE-INFO.txt`

## 8. Deployment on a box with no toolchain

The target is a game-panel container: python present, cargo and node absent, one command, and a
container that exits looks to the panel like a crash to be retried forever.

- [x] `crates/aerona` - one binary running the dashboard, the API and the gateway worker in one
      process (a panel runs one command); `aerona-api` + `aerona-bot` stay available for a
      supervisor that wants to restart them separately
- [x] `stop::Stop` - a latch both halves wait on: one-shot, survives a late subscriber (a plain
      `watch` channel drops the value when nobody is listening, which turned a graceful stop into
      a kill -9), driven by SIGTERM / SIGINT / Ctrl-C
- [x] the worker is retried in-process on a 3–60 s backoff, so a Discord outage or a rejected token
      never takes the site down; release builds `unwind` instead of `abort`, which is what makes
      the worker's per-event panic isolation real
- [x] `AERONA_BOT=off` - dashboard with no worker, and the only state in which the token stops
      being required; the key pepper then comes from `data/install.key` (0600) instead of the token
- [x] same-host `Origin` fallback while `AERONA_BASE_URL` is unset (IP literal or `localhost`,
      names refused) so a fresh panel install can log in at `http://ip:port`; `main.py doctor`
      prints the fix when it is not that case
- [x] `main.py` is a launcher first and a builder second: prebuilt binary wins, a build is the
      fallback, and `python3 main.py` with no subcommand is `run`
- [x] first boot writes `.env` from `.env.example` with a generated `AERONA_MASTER_KEY` (0600),
      never invents a token, and comments the placeholder out when the worker is off
- [x] `aerona doctor` - config answers, whether the port can be bound, whether `dashboard/dist` is
      a real prerendered build, and what Discord says about the token right now (exit 6 when refused)
- [x] `check` prints the intent *names* it asked for, not `GatewayIntents(512)`
- [x] `bin/aerona-linux-x86_64` - statically linked musl build shipped in the archive, so no glibc
      assumption and no toolchain on the box; `main.py build` is a no-op next to it
- [x] `scripts/build-static.sh` - the reproducible packaging build (musl target, `musl-gcc` linker,
      and an opt-level override because a 2 GB packager with no swap spends its time in direct
      reclaim at `opt-level = 3`); it refuses to write a binary that has a `PT_INTERP`
- [x] verified against the shipped artifact in a clean directory: `check` finds `bin/aerona-linux-x86_64`
      and reports the prerendered dashboard; boot writes `.env` (0600) and starts the static binary;
      migrations apply; pairing code printed; `key create --out` → bearer `GET /guilds` 200;
      `/.env`, `/.git/config`, `/../etc/passwd` and `/dist/.template.html` 404; assets `immutable`,
      HTML `no-cache`, API `no-store`, headers present; `healthcheck` rc 0; SIGTERM drains in 5 s, exit 0
- [x] three bugs that verification found and fixed:
      `--version` used to run *after* `Config::load`, so on a box with no `.env` the launcher's
      usability probe saw a config error (and would have reinstalled a toolchain);
      `cookies_secure()` marked the session cookie `Secure` for `http://localhost`, which browsers
      allow and every other client does not - `main.py` could pair but then 401'd forever, so it is
      now https-only;
      and the launcher used to write `AERONA_BASE_URL=http://localhost:<port>` into a first-boot
      `.env`, which switches the same-host fallback off and 403s a real browser on the panel's IP
- [x] `main.py` sizes cargo's `-j` from `MemTotal` (1 under 3 GiB, 2 under 8) instead of a
      workspace-wide `jobs` in `.cargo/config.toml`: two release rustcs at ~800 MB each is an OOM
      kill on a 2 GB container, which the panel would read as a crash loop. `AERONA_BUILD_JOBS=n`
      (or `auto`) overrides; a hand-run `cargo build` on a workstation keeps cargo's own default
- [x] `dashboard/package.json`: `@types/node` moved to `^22.20.0`, because vite 8 requires
      `^20.19.0 || >=22.12.0` and the old exact pin made `npm ci` fail with ERESOLVE - a fresh
      clone could not build its own site
- [x] `AERONA_BASE_URL` is left *empty* in `.env.example`, in `DOTENV_TEMPLATE` and in the `.env`
      first boot writes; an `http://localhost:PORT` placeholder there is a trap, not a default
- [x] a real install on a game panel (Altare, `parkervcp/yolks:python_3.14`, one allocated TCP
      port, no TLS in front) booted from the shipped archive: `.env` written, migrations applied,
      pairing code printed, dashboard + API + worker in one process, `PORT` honoured - and it
      surfaced two things the sandbox could not:
      **`upgrade-insecure-requests` in the CSP** puts the page in protected mode, so a browser
      rewrites the app's own `http://` asset and API requests to `https://` on a port with no TLS
      and the operator sees a blank page; removed, with the reasoning and a test, since every
      source is `'self'`, `data:` or an explicit `https://` host and there is nothing to upgrade.
      And **a named host with `AERONA_BASE_URL` unset is refused by the origin gate** (correct -
      rebinding), which the README now says out loud next to the "browse over `http://`" line,
      because `https://panel-node:port` is what everybody types first.
- [x] the worker's startup line prints intent *names* too, not `GatewayIntents(519)`
- [ ] `bin/aerona-windows-x86_64.exe` / `bin/aerona-macos-*`: not cross-buildable from here, and
      the launcher's auto-build path covers them instead

## 9. Dark redesign, the localhost redirect, and the http refusal

Prompt: *"the redirect goes back to using the localhost thing… make it fully dark mode, SEO
ready, neo-brutalism with iOS-7 blur, fully customizable and clean, fix all other bugs, then
zip it up."* Four separate things were hiding in that sentence.

- [x] **The localhost redirect was real, and it was in `redirect_uri`.** `Config::base_url`
      falls back to `http://localhost:PORT`, which is right for the origin *check* and wrong for
      an absolute URL handed to a browser: clicking "sign in with Discord" on a panel sent
      `redirect_uri=http://localhost:25154/api/v1/auth/discord/callback` to Discord, so the
      bounce landed on the operator's own machine. Fix: `Config::request_origin(host, proto)` -
      the configured base URL always wins (a `Host` header must never steer an authorization
      code), otherwise the request's own host is reflected *only* under `host_is_self` (IP
      literal, `localhost`, `*.localhost`), and a name nobody configured returns `None` → the
      route answers `302 /pair?error=origin-unset` instead of guessing. The host-splitting that
      `allows_same_host` did by hand is now that one function, shared.
- [x] **`main.py` refused to start on exactly what its own README told you to write.** The
      "plain http on a public host" entry in `Config.problems()` moved to `notes()` with honest
      wording - the second half of the old message ("the CSRF origin check will reject real
      browsers") was inverted: setting the http URL is what makes the check *pass*. Trailing
      slash stays fatal. Launcher test rewritten to lock the new contract (36 → 36, OK).
- [x] **Dark neo-brutalism, token-for-token** (`docs/DESIGN.md` is now the source of truth;
      `uploads/DESIGN_GUIDELINES.md`'s colour rules are superseded, its structural ones are not).
      2px `--color-line` borders, hard offset shadows at 55 % black, `--radius: 3px`,
      `--color-canvas: #0d0f12`, `.glass*` kept as the floating set with
      `saturate(180%) blur(var(--blur))`, and the light palette demoted to a
      `[data-theme='light']` override block. Every `rounded-[NN]`, `bg-white/70`, `text-white`
      and `shadow-[…]` literal in `src/` is gone - 14 files touched mechanically.
- [x] **Contrast, measured rather than claimed.** `npm run check:design` (in `build`) computes
      WCAG ratios for 15 sanctioned text/non-text pairs in both themes from the token table,
      plus a token-integrity pass over `src/`. It found three failures on its first run: white
      on `#ff6a2b` (2.86:1, light and dark), `ink-faint` on `sunken` in light (4.38:1), and the
      accent as a focus ring on a light canvas (2.73:1). Fixed with a `--color-ring` token
      (burnt orange in light), a dark `--color-on-signal`, and a darker light `ink-faint`.
- [x] **Customizable in three layers**: per browser (theme mode, accent, radius, vibrancy in
      `localStorage`, with a `?`-free pre-paint bootstrap at `public/theme-boot.js` - a separate
      *file* because the document CSP is `script-src 'self'` with no nonce, so an inline script
      would be silently blocked); per install (`dashboard/dist/theme.css`, loaded after the
      bundle by that same script, editable on the server with no build); per fork (`@theme`).
      `--color-signal-deep`/`-wash` are `color-mix()` derivatives of `--color-signal`, so one
      variable re-colours everything. `prefers-reduced-transparency` zeroes `--blur`.
- [x] **SEO**: `og:locale`, `og:type` per page kind, `article:modified_time`, `og:image:alt` +
      `twitter:image:alt`, `application-name`, `color-scheme`, three `theme-color` entries,
      `sameAs`/`image`/`inLanguage` in the JSON-LD graph, `lastmod` in the sitemap from the
      build clock, `Disallow: /api/` in robots. A `noindex` page no longer gets a canonical or
      an `og:` block. The prerender audit now *enforces* most of it - including "exactly one
      `<h1>`", "title in 12–70 characters", "the theme bootstrap is linked", and
      "`robots.txt`/`llms.txt` came out as multiple lines".
- [x] **Bug class found on the way**: a `join('\\n')` in `prerender.mjs` would have written a
      one-line `robots.txt` - checked, and the tokens in *this* file are single-backslash, so it
      was fine; the audit is there now so the next one cannot pass unnoticed.
- [ ] `main.py` does not yet print the panel's own URL next to the sign-in hint; it prints a
      relative `/pair/` on purpose, since the only host it can know is `127.0.0.1`.

## 10. The repair round after the dark flip

The four headline items in §9 shipped, then the small print was gone through line by line. What
changed, and what was found to be wrong in the *checks* rather than the product:

- The panel self-test moved into the repo as `scripts/verify-panel.sh` (it was a scratch file at
  `/tmp/verify_panel.sh`, which is how a harness ends up asserting nothing). It now runs 34
  assertions and dials loopback with the host under test in a `Host:` header instead of letting
  curl resolve the name - an earlier draft resolved a real public address, so a green run there
  was luck rather than evidence. Servers are started with `setsid`, every request carries
  `--max-time`, and a check against an empty response reports FAIL instead of passing vacuously.
  Result against the shipped binary and build: 34 passed, 0 failed, 0 skipped.
- `main.py --help`/pairing text and the Docs page still named `aerona-api` after the binaries were
  unified; both now say `python3 main.py pair`.
- `dashboard/public/og-image.jpg` was a bad render (right column missing, tagline clipped).
  `scripts/make-og.py` draws it from the tokens *and measures every string*: `Layout.text` records
  each box and fails the run with the offending number if anything would leave the canvas, leave
  the card, or overflow its line budget. The clipped right column is now an error, not a review
  note. The `60 ROUTES` claim on it is backed by `assert_eq!(routes.len(), ROUTES.len())` in
  `api/app.rs` - the table really has 60 entries.
- The share image's glow used to be a 180px blur at 1200×630, which took enough RAM to get a
  concurrent `cargo` build OOM-killed in this sandbox. It is blurred at 1/8 scale and stretched
  back, which is the same Gaussian at that radius.
- `site.webmanifest` (splash + theme colour) and `favicon.svg` were still light-mode `#fafaf8`;
  both carry the dark tokens now, so the install-time artefacts match the app.
- `SECURITY.md` gained the paragraph the launcher's behaviour implies: what plain `http://`
  exposes on a panel port, why the cookie is deliberately written without `Secure` there, the three
  ways to get real TLS, and the rule the OAuth redirect follows (configured origin, else IP or
  `localhost` from `Host`, never an unconfigured name).
- `.env.example` and the first-boot template now describe `AERONA_BASE_URL` the way the code
  behaves - the exact origin in the browser, port included, unset meaning the host you actually
  reach - and document `AERONA_SITE_ORIGIN` as the build-time counterpart, which `main.py` never
  reads.
- `docs/DESIGN.md` speaks of `.surface`; `.band`/`.card` in the guidelines document are that
  document's own vocabulary and were never class names here, so the "card border contradiction"
  was a false alarm - the card really is 2px `--color-line` on paper, which is what a dark canvas
  needs to stay legible as an elevation rather than a smudge.
- Re-verified after the edits: `cargo fmt --check` clean, `cargo clippy --workspace --all-targets
  -D warnings` clean, `cargo test --workspace` 267 passed / 0 failed, launcher 36 passed,
  `npm run build` green including the design audit (15 contrast pairs × 2 themes) and the SEO
  audit (18 routes, 7 indexable), `scripts/verify-panel.sh` 34/34.
- `main.py`'s `cargo_binary()` preferred a cargo found on `PATH` over the toolchain the launcher
  installs itself under `.cache/cargo/bin`, while `toolchain_env()` still points `RUSTUP_HOME` at
  the local (empty) directory - so a source install *with* a system rustup would have failed to
  build with "no installed targets", and the test for it failed only when the machine running the
  suite happened to have cargo on `PATH`. The local install wins now, `PATH` is the fallback, and
  the launcher suite passes identically with and without a system cargo (36/36 both ways).
- Delivery note: this sandbox has no `.git` in the tree and no `/workspace` mount, so there is no
  commit hash to name here and the archive is built from `/home/user/aerona` by
  `scripts/release-zip.py`; the binary's `--version` correspondingly reports the version and
  protocol numbers without a commit, which is what `build-static.sh` does when git is absent.

## 11. The light panel, and the redirect URI that could not be diagnosed

Two reports arrived together: Discord sign-in still answered `Invalid OAuth2 redirect_uri`, and
"this is not what I want" about the panel's look, with two reference screenshots and a Dribbble
shot (an analytics dashboard: icon rail, big tabular numbers, a chart band, a right-hand list of
action rows with meters). Both are treated here because both were about the same thing - the panel
was unreadable in the ways that mattered.

### The OAuth half

`Invalid OAuth2 redirect_uri` is not an error the product can catch: Discord renders that page in
the user's browser, from a request that never reaches the install. There is no callback hit, no
log line, nothing in JSON. So the fix is not to interpret it better, it is to remove the guessing
before it happens.

- `AERONA_OAUTH_REDIRECT_URI` pins the string. When it is set it is used **verbatim for both halves
  of the flow** - `state` now carries the `redirect_uri` that was offered at `/authorize`
  (`SCHEMA_V3`: `ALTER TABLE oauth_state ADD COLUMN redirect_uri TEXT`), and the token exchange
  replays that stored value instead of recomputing one from the second request's `Host`. A proxy
  that rewrites `Host`, or a `.env` edit between the two hops, used to turn a good code into
  `invalid_grant`; the pre-v3 rows fall back to derivation, which is why the column is nullable.
- `/pair` shows the exact value this install will send, with a copy button, the portal path to
  paste it into, and the three shapes that fail with an otherwise correct setup (a trailing `/`, a
  query string, `http` on a non-local host). It builds the *derived* string locally from its own
  origin - `GET /api/v1/status` echoes `redirectUri` only for `pinned` and `base-url`, because
  reflecting a `Host`-derived URI through an unauthenticated JSON endpoint is an oracle.
- `python3 main.py check` and every `run` print the same sentence from `Config::notes()`, plus a
  separate one when `AERONA_DISCORD_CLIENT_ID` is set without the secret (a half-configured login
  button that used to fail silently).
- `Config::redirect_uri_guidance` is the single source for all three surfaces, and
  `aerona-api` exposes it as `oauth::redirect_guidance`.
- Tests: `oauth_state_is_single_use_and_replays_the_redirect_uri` (round-trip, and `None` stays
  `None` so nothing is invented); `tests/test_launcher.py::test_the_redirect_uri_is_said_before_the_first_login`
  (pinned note, derived note, half-configured note, silence when no application is configured);
  `scripts/verify-panel.sh` gained a section that boots with a pinned URI, dials it from the *IP*
  and asserts the Location still carries the pinned value, plus a check that `main.py`'s
  `DISCORD_CALLBACK_PATH` is the route the API actually mounts.

### The panel half

- `index.css` is light-first: the light palette is what `@theme` declares, and the dark palette -
  the same values as §9, kept intact - moved into `[data-theme='dark']`. `--edge` 1px,
  `--radius` 12px (slider 0–16), two-layer blurred shadows, `color-scheme` per theme. `.op-*` is a
  new scoped layer (50 class names) for the three things a marketing page never needs: a fixed rail,
  a sticky table head inside a card, and a number read at a glance.
- The old structural class names (`.surface`, `.surface-well`, `.press`, `.lift`, `.glass*`,
  `.rounded-brutal*`) were *kept* and re-tokened instead of renamed, which is what re-skinned the
  nine views and the marketing pages in one pass; `.op-*` only carries structure that had no
  previous shape. This supersedes §10's note that a card is "2px `--color-line` on paper": it is
  1px now, in both themes.
- `components/ui.tsx` grew the parts the reference is made of - `MetricCard` (count-up on
  `--ease-out-expo`, reduced-motion aware, `op-delta` from the sign of a fraction), `BarChart`
  (peak-marked, hover readout), `ProgressRow`, `ActionRow`, `Tile`, `PageHead`, `Kbd` - and
  `Panel`/`Table`/`Tabs`/`Button` emit `op-*`. `Button` gained `variant="ink"` for the panel's
  primary action, which is why `--color-on-ink` exists as a token and is in the contrast table.
- `components/layout.tsx` owns `OpChrome`: a 68px `op-rail` built from `site.appNav` with Lucide  <!-- superseded: the rail is a labelled 232px sidebar with count badges, and the panel paints on a neutral-500 field with a rounded sheet; see §12 and `docs/DESIGN.md` -->
  glyphs and `::after` tooltips, a sticky `op-top` with the guild switcher, the theme cycle and a
  ⌘K "jump to" filter over the same nine routes. `View` in `pages/app/shared.tsx` is the only
  place that frame is built, so no page can drift to a different width or action set; `App.tsx`
  drops the marketing header for app routes (but not when there is no session, where the header is
  the way out) and hands `<main id="main">` to `View` in that case - the prerender audit caught the
  first version of this leaving the skip link pointing at nothing on all nine `/app` documents.
- Icons are `lucide-react`, added to `package.json` like Tailwind and Framer Motion before it, so a
  rebuild of the panel needs only `npm install`; the icons themselves are compiled into the bundle.
- `Overview` is the reference implementation: four metrics, real messages-per-minute bars from
  `GET /stats/bot`, a "do next" action list, gateway figures, then the two ledgers in flush cards.
  It also fixed a live bug: the client asked for `?hours=6` while the endpoint reads `points`
  (default 120), so the series was silently the wrong window; `statsSeries(process, points)` now
  clamps to the server's own 2–2000 bound.
- `theme-boot.js`, `index.html`, `site.webmanifest`, `favicon.svg` and `head.ts`'s `color-scheme`
  all moved to the light default together; `verify-panel.sh` asserts the boot script's default and
  both seeded canvases against what ships, since a disagreement there is a flash on every install.
- `scripts/check-design.mjs` had to be re-pointed, not relaxed: it read `@theme` as "the dark
  palette", which after the flip meant it measured the light palette twice and called one copy
  dark. It now reads `@theme` as light and merges every `[data-theme='dark']` block for the dark
  column, and the pair list grew to 18 - including `ink-mute`/`ink-soft` on the sunken table band
  and `on-ink` on `ink`. Fixing it exposed four genuine light-mode failures, which is why the light
  ink ladder moved (`ink-mute #5a6270`, `ink-faint #69707e`, `ink-dim #868d9b`) and `--color-line`
  to `#d8dce3`: the audit still fails if anyone lightens them again.
- Re-verified after every edit in this section, against the tree as shipped: `cargo fmt --all --
  --check` clean; `cargo clippy --workspace --all-targets -D warnings` clean; `cargo test
  --workspace` 268 passed / 0 failed; launcher suite 37 passed / 0 failed; `npm run build` green,
  design audit (18 contrast pairs × 2 themes, 37 tokens) and SEO audit (18 routes, 7 indexable)
  both clean; `sh scripts/build-static.sh` produced a stripped static-pie `bin/aerona-linux-x86_64`
  with no `PT_INTERP`; `scripts/verify-panel.sh` 54 passed / 0 failed / 0 skipped, which now covers
  the pinned-URI behaviour, the panel chrome in the shipped stylesheet, and the three theme
  defaults agreeing. The archive from that tree is what was uploaded.

## 12. Pairing removed, Discord as the only door, and the panel rebuilt on a reference

Why this round happened: `can_manage` - the function that decides whether a Discord account may
administer a server - had **no production caller**. It was defined, unit-tested, and never used, so
in practice any account that completed OAuth got a session that could moderate every guild the bot
served. The pairing code was the thing standing between that sentence and a bug report, and
removing it forced the check to be written where it belongs.

- [x] **The pairing code is gone from the product** (this round). `POST /api/v1/pair`,
 `POST /api/v1/pair`,
      `bootstrap.rs`, `aerona pair`, `main.py pair`, `key pair-code` and `pages/Pair.tsx` were
      removed, not hidden; `/login/` is the only sign-in page and Discord the only door. The
      reason is in `SECURITY.md`: `can_manage` had no caller, so *any* Discord account that
      finished OAuth could moderate every guild the bot served. Now `finish_login` mints a
      session only for an account that manages a guild in `guilds`, stamps that list into
      `sessions.guild_grants` (v4 migration), and `require_guild` refuses anything outside it.
      `pairing_codes` the table stays - see `SECURITY.md` for why dropping it is the worse call.
- [x] `sessions` gains `guild_grants` (schema v4, `ALTER TABLE sessions ADD COLUMN guild_grants
      TEXT`): the JSON list of guild ids the account managed at sign-in. `NULL` is unrestricted,
      which is what keeps every pre-v4 session - and `testutil`'s minted ones - working without a
      data migration. `require_guild` consults it on every guild-scoped request.
- [x] The authorize request is the shape that works, verified by test rather than by eye:
      `?client_id=…&redirect_uri=<origin>/auth/callback&response_type=code&scope=identify+guilds
      &state=<43 url-safe chars>` - `+` not `%20`, no `code_challenge`, no `prompt=none`, and the
      token exchange sends no `code_verifier`. `oauth::authorize_url` is a pure function so the
      exact string is asserted, not described.
- [x] `/auth/callback` is the mounted path; `/api/v1/auth/discord/callback` answers as a
      `301` that carries the query, so an install whose portal entry predates the move still
      signs in. `main.py`'s `DISCORD_CALLBACK_PATH` and `status.callbackPath` say the same thing,
      and `verify-panel.sh` fails if the three ever drift.
- [x] `AERONA_PAIR_TTL_SECS`, `Config::pair_ttl_secs`, `bootstrap.rs`, `Command::Pair`,
      `--pair`, `main.py pair`, `key pair-code`, `pages/Pair.tsx` and `pairApi` are all deleted -
      not hidden behind a flag. `/login/` replaces the page.
- [x] The panel is the reference: 232px labelled sidebar with count badges, one rounded sheet
      holding an underline tab row, four 24px `op-panel` paints (ink bars + right-edge values,
      violet donut + percentages + legend, lime area + month axis, grey action + icon tiles), and
      an alerts list whose important row is the dark one. `neutral-500` (`#737373`) is the field
      instead of white, and it is one token: `--sx-canvas`. `check-design.mjs` measures nine
      `--sx-*` pairs in both themes and pins the anchor hex, because on a mid grey the only ink
      that clears 4.5:1 is pure white - which is why the rail is written in white.
- [ ] `verify-panel.sh` cannot assert a real Discord round trip, so the cookie attributes on a
      minted session are checked in `auth.rs` instead; the shell script proves the *negative*
      (404 on `/api/v1/pair`, `aerona pair` exits 2, no `data/pair-code`, no `pair` in the
      launcher) and the URL shape. A fixture that drove a stub OAuth provider end to end is the
      missing piece.
- [ ] `pairing_codes` the table and its four `Db` functions stay (see `SECURITY.md` for why
      dropping a column is the worse trade). A future release that is willing to require a
      re-migration can delete them together with the hourly `purge_pairing_codes` call.

## 13. Dashboard-only: the front page is gone, and sign-in had a fatal bug

**What changed.** Three things, in the order they were asked for.

1. **`discord rejected the action (502): no user id`, reported from a live install.** Not a
   Discord problem at all: `finish_login` called `exchange()` and then passed the *token* body
   to `parse_profile()`. The token endpoint answers with `access_token`; `id` and `username`
   come from a second request, `GET /users/@me`, which this code never made. Every sign-in on
   the planet failed the same way, and because `parse_profile` returned a synthetic
   `Error::Discord { status: 502, message: "no user id" }`, the log said something about Discord
   instead of "we did not ask". `oauth::upstream_login()` now does the three calls in order -
   token, profile, guilds - and `a_token_body_is_not_a_profile` pins the distinction.
   Non-success from the token endpoint is split into `invalid_grant` (a retry, 400) and
   `invalid_client` (a broken `.env`, and the message says so), and a body that is not JSON at
   all - a proxy or an edge answering 502 with HTML - logs the first 200 characters rather than
   becoming "start again".
2. **Nothing in the sign-in flow puts a JSON error body in a browser.** A callback with no
   code, a used or expired `state`, a login whose account administers no server, and any
   upstream failure now 303 to `/login/?error=<fixed token>` (slashed: the route the static layer
   actually owns - see §14), which renders one sentence and a button. `tests::an_unfinished_callback_bounces_to_the_page_not_to_an_error_body`. The
   harness needed one fix for this to be testable at all: `Site`'s reqwest client now sets
   `Policy::none()`, because a client that chases redirects turns every 302 into whatever the
   target says and hides the header under test.
3. **The dashboard is the whole product.** `Home.tsx`, `Features.tsx`, `Commands.tsx`,
   `Docs.tsx`, `Security.tsx` and `Legal.tsx` are deleted, along with the marketing header,
   footer, sunrise and `llms.txt`, and the unused primitives in `ui.tsx` (`SectionHeader`,
   `Card`, `Lead`, `TwoTone`, `ArrowLink`, `StatGrid`, `TabPanel`). Routes are no longer under
   `/app`: `/`, `/servers/`, `/moderation/`, `/tickets/`, `/stickies/`, `/raid/`, `/filter/`,
   `/audit/`, `/settings/`, `/access/`, `/login/`, `/404/` - 12 prerendered documents, all
   `noindex`, `robots.txt` = `Disallow: /`, an empty sitemap. `verify-panel.sh` asserts the
   old paths answer 404 so the route table cannot grow back quietly. The status strip that
   reported websocket latency, RSS, db size and the outbox under every view is gone: it was
   instrumentation for whoever builds the thing, not information for whoever runs it, and
   `Access` still shows all of it.

**The server list, public-bot style.** `pages/app/Servers.tsx` shows every guild Discord said
this account administers (`ADMINISTRATOR` or `MANAGE_GUILD`), whether or not Aerona is in it.
The rows the bot is in get `Open`; the rows it is not in get **Add the bot**, a link to
`oauth2/authorize?client_id=…&permissions=8&scope=bot&guild_id=…&disable_guild_select=false`
built by `oauth::bot_invite_url()` - one link per server, with that server preselected and
still changeable, and no `response_type` because there is nothing to redirect afterwards.
`GET /api/v1/guilds` is the merge of two sources: the `guilds` table for what the bot serves,
and `sessions.guild_grants` for the servers this session's operator administers, which is why
the column now holds `{id, name, icon}` objects instead of bare ids (`set_session_grants` takes
a `Value`; `session_allows_guild` reads numbers *or* objects, so an older row still parses and
still means the same thing). The login gate is one rule: administer at least one server. The
old second condition - *and this install must already serve it* - was what made a fresh
install unloggable-in-advance, and it is dropped deliberately; `require_guild` still refuses
any guild with no row or no bot presence, so "I can see it but not change it" is the honest
state for a server that has not been invited yet.

**Open.**
- [ ] A freshly invited server appears in the panel as soon as the bot's `GUILD_CREATE` writes
      it, but nothing *tells* the operator: they press Refresh. A 60-second poll on `/guilds`
      while the Servers page is open, or a gateway-event-driven toast, is the fix.
- [ ] Re-checking admin rights happens at sign-in only. A session keeps its grant list for its
      lifetime (see §12), so losing Administrator in Discord costs nothing until the cookie
      expires. The honest UI answer is a line on `Access`; the honest code answer is a
      re-check when a write is refused.
- [ ] `permissions=8` for invites. It matches what the product's features need and what the
      access rule assumes, and it is also the ask that makes some operators say no. A granular
      list, or both buttons, is a decision for somebody with real install feedback.
- [ ] `index.css` still carries the marketing rules (`.op-hero`-adjacent bands, the sunrise,
      `.display`) that no page uses. They are dead weight in a 1670-line stylesheet, and pruning
      them means re-checking every panel primitive that borrows a token from the same block.

## 14. The 404 that was not a 404, the registry contract, and the reference's paint

**What changed.** The sign-in bounce was arriving - and rendering as `404 Not Found`.

1. **`static_headers` stomped a handler's status.** `GET /auth/callback` is a root-level route, so
   no file on disk matches it; the `ServeDir` fallback answered, and the middleware that decorates
   every response with the CSP and the cache headers rewrote the 404 *over* the 303 the handler had
   already returned. The headers of the failed response made the bug obvious in one line:
   `HTTP/1.1 404 Not Found` with `location: /login?error=expired-link` on the same message. Fixed by
   deriving `ROOT_PATHS` once and exempting exactly those paths from the fallback's status rewrite
   (`owned_by_the_router`), and by bouncing to `/login/?error=` - the slashed form is the prerendered
   route, so the target resolves whether or not a build is mounted.
   `the_sign_in_bounce_survives_the_static_layer` pins it with a `TestEnv::write_dist` that puts a
   real `dist/` in front of the router, which is the arrangement that broke; deleting the exemption
   makes it fail with `left: 404, right: 303`.
2. **The panel now speaks shadcn's vocabulary as well as its own.** `lib/utils.ts`'s `cn` is
   `twMerge(clsx(inputs))`, so a caller's `className` beats a recipe instead of being appended to
   it; `src/components/shadcn.tsx` ships the registry's shapes (Button, Badge, the Card family,
   Input, Label, Separator, Switch, Skeleton, Tabs, Tooltip) as `cva` recipes that emit `data-slot`;
   the slot rules live at the end of `index.css` and are wrapped in `:where()` so a class the page
   chose always outranks a default; `@custom-variant dark` points `dark:` utilities at the `.dark`
   class `apply()` keeps in step with `data-theme`; `tw-animate-css` is imported for the open/close
   classes copied components expect. The contract itself - `--background`, `--card`, everything as
   HSL channel triples - is generated from the panel's hexes, and `check-design.mjs` now converts
   each triple back to a hex and fails the build if it disagrees with `--sx-source-*`, in either
   theme, which is the check that caught three real drifts the day it was written (a light `--card`
   that painted #e6e6e6 while the sheet said #e5e5e5, a `--radius` frozen at 12px in one block and
   14px in the other, and a `data-slot` styling a popover nothing emits).
3. **The shimmer was not rendering at all.** The panel's sweep painted itself with
   `hsl(var(--sx-on-canvas) / 0.06)` - `--sx-on-canvas` is a hex, `hsl()` around a hex is invalid,
   and an invalid declaration disappears at computed-value time, so loading blocks were flat grey in
   the panel and shimmering everywhere else. Now `color-mix(in srgb, …)`, which takes any colour;
   the band is a skewed specular edge rather than a colour change, 1.55s, staggered 90ms per `--i`
   (`Skeleton` sets it per row) so a five-row list reads as five arrivals; `prefers-reduced-motion`
   stops the sweep and leaves a still highlight, and the audit asserts all three.
4. **Dark is the shipped default, and it is the reference's palette, measured.** A colour census of
   frames pulled from the video the panel was pointed at: canvas `#080808`, card `#0a0a0a`, sidebar
   `#171717`, hairline `#242428`, ink `#f5f5f5`, the primary pill `#e3e3e3` with `#18181b` on it,
   state green `#36d399` and amber `#fbbd23` reserved for deltas. Radii 14/9px, `--shadow-card:
   none` in dark - a card is a border there, not a shadow. `--sx-card-ink` flipped from an ink plate
   to a light pill, which is why `.op-badge`, `.op-avatar` and the ink button no longer type `#fff`:
   they read `--sx-on-card-ink` / `--sx-card-ink-hi`, and the audit pair that used to say
   "white on ink" says "the ink on a filled plate" and clears 4.5:1 in both themes. `DEFAULTS` in
   `lib/theme.tsx`, `public/theme-boot.js` and `index.html`'s `data-theme` are the same answer, and
   `verify-panel.sh` fails if any one of the three drifts.

**Verified on the tree as shipped here.** `cargo fmt --all --check` clean; `cargo clippy --workspace
--all-targets -- -D warnings` clean; `cargo test -j4 --workspace` **269 passed / 0 failed** (core 144,
api 61, bot 43, `aerona` 19 + 2 that exec the real binary); `npx tsc --noEmit` clean; `npm run build`
→ 12 routes (0 indexable), 12 JSON-LD graphs, design audit clean (18 site + 9 panel contrast pairs ×
2 themes, 121 tokens declared); `sh scripts/verify-panel.sh` **77 passed / 0 failed / 0 skipped** - up
from 73, all four new assertions on the callback - from the repo *and* from the extracted archive;
`python3 main.py` boot from that archive, `python3 -m unittest discover tests` for the launcher.

**Still open.**

- The reference's charts animate themselves in (an area path drawing, bars rising, a donut sweeping).
  The panel's SVG charts are static. The primitives for it are all in CSS (`stroke-dasharray` +
  `pathLength`, `transform: scaleY()` on a `transform-box: fill-box` bar, `stroke-dashoffset` on the
  donut), so this is a 30-line patch and not a redesign.
- `--sx-on-card-ink` covers the plate-and-ink pairs that exist today. A component that pairs a
  literal `#fff` with `--sx-card-ink` again would only be caught by the audit if the pair is added
  to `PANEL_PAIRS` - worth a lint that greps for `background: var(--sx-card-ink)` without a
  `color: var(--sx-on-card-ink)` within the rule.
- `scripts/make-og.py` still renders the retired marketing hero, so `public/og-image.jpg` shows a
  page that no longer exists. Nothing is indexable and no page references the image as a preview of
  a live URL, so it is inert - but it is wrong, and regenerating it from the panel would be honest.
- `index.css` still carries the marketing-era rules (`.display`, the hero, the band, the sunrise
  pointer) that no page uses since `/` became the panel. Deleting them means deleting the `@theme`
  primitives they were measured with, so the audit's "18 site pairs" section goes too.

## 15. One palette: the light theme retired, the colour-blocked cards neutralised, the chart draws itself

Three decisions, taken together, because they are the same decision: the panel is the product and
it has one face.

1. **The light theme is gone, not hidden.** `@theme` declares the reference palette directly, so
   the `[data-theme='dark']` override block, its `--sx-*` twin and the ~25 `[data-theme='dark']
   .op-*` re-paints it needed were all folded into the base rules and deleted. Every hard-coded
   light literal in the panel layer (`#f7f7f8` cards, `#fff` hovers, `rgba(255,255,255,.1)` rails)
   became a token - `--sx-plate`, `--sx-hover`, `--sx-series-1..3` - so there is nothing left to
   override, and the glass material (`--glass-fill`, `--glass-edge`, the bar/sheet/pill trio) is
   authored for a near-black field once instead of twice. `ThemeMode`, `MODES`, `ThemeCycle` and
   the settings' Theme row are deleted with it; `data-theme='dark'` + `.dark` stay on the document
   only so a copied component's own `dark:` utilities resolve, and the audit fails if a second
   `--color-*` block reappears. Elevation is now hairlines by construction (`--shadow-card: none`).
2. **The four colour-blocked cards went neutral.** `op-panel--ink/violet/lime/grey` painted violet
   and lime panels with `#171717` text - measured 4.5:1 against the *light* sheet they were
   designed on, and now a hole in a near-black page. They share the card paint, and colour is left
   to what carries meaning: a delta, a status dot, the one light pill. The donut and the area chart
   got `--sx-series-*`, and their slices stopped being hexes typed in `Overview.tsx` (an SVG
   `stroke=` presentation attribute cannot take `var()`, so the component sets `style.stroke`).
   `tone` survives as a slot for a fork that wants its own fill.
3. **The area chart draws itself in**, the way the reference's do: `clip-path: inset()` wiping
   left-to-right over 760 ms, the fill coming up 300 ms later. It is a wipe on the `<svg>` rather
   than `pathLength` on the path, so no geometry has to cooperate and nothing measures anything; a
   re-render replays it, which is the honest behaviour for a chart with new numbers. Bars and the
   donut stay static on purpose - a value you are comparing must not be moving while you compare
   it - and both the wipe and its `prefers-reduced-motion` guard are asserted by the audit, so
   deleting either fails the build rather than quietly changing the product.

Also fixed on the way: the head emitted `color-scheme: light dark` (form controls painted light for
a light-OS reader on a dark field, one frame of it), the accent input fell back to the retired
ember hex while the shipped accent is `#e3e3e3`, `UiProvider` still listened to
`prefers-color-scheme` for a mode that no longer exists, and `theme-boot.js` branched on
`p.mode`, which nothing can set any more.

**Verified here:** `npm run build` (tsc + design audit: 18 surface + 9 panel contrast pairs, one
palette, 124 tokens declared), 12 routes prerendered, `sh scripts/verify-panel.sh` 77/0/0 against
the binary from §14 - Rust is untouched by this round, so `cargo test --workspace` 269/0 and
clippy `-D warnings` still describe `bin/aerona-linux-x86_64` at md5 `aeacf5bd50e59885a42bb0c5e252f528`.

**Now open:** `index.css` still carries the ember/sunrise hero block (`.hero-*`, `.sunrise-*`,
`@property --sx`) that no component references - TODO's oldest item, and the only place a stale
`#f4f5f7` survives in the file (in a comment). Deleting it is a 90-line cut with the audit's
`--sx` exception going too. The donut's third series is intentionally below 3:1 and measured
nowhere; if it ever becomes a mark you read rather than a band you compare, it needs a pair.

## 12. The shadcn panel, per-server everything, and the guards that enforce

The design round was voided: the panel is now the `npx shadcn@latest init --preset bpBC
--template vite` output - React, Tailwind, Framer Motion, and **only** registry components for UI.
That deleted `pages/app/*`, `AppShell.tsx`, `components/{ui,shadcn}.tsx`, `lib/theme.tsx`,
`scripts/check-design.mjs` and `public/{theme-boot.js,theme.css,fonts/}`: one palette, an accent
slider, a radius slider and a vibrancy switch went with them, along with the 90 lines of hero CSS §11
left open. `index.css` is now the preset's tokens plus `--radius` and `tw-animate-css`; light and
dark both exist again, chosen by an inline script before first paint and by a `ThemeToggle` in the
header, and the only state is `localStorage['aerona.theme']`.

**The root is the server list.** `pages/Guilds.tsx` shows every guild Discord said this account is
in, the ones with the bot in them first, each row carrying the invite link (`scope=bot%20
applications.commands`) that adds it. Opening a server the bot is not in is not a disabled form: it
is `BotNotHere`, whose only button is that invite. The guild switcher in the rail is the same list,
and `report`/`overview` are fetched once per server page by `lib/guild.tsx`.

**A URL that names a server has to answer.** `/guild/<id>/<page>/` has no file behind it - the id is
in the path - so the static layer now owns the prefix: `APP_PREFIXES` + `owned_by_client_router()`
in `routes/system.rs`, consumed by `app.rs::static_headers` (a parametric client URL is no longer
downgraded to the 404 document) and mirrored by an explicit
`.route("/guild/{*rest}", any_service(ServeFile::new(dist/index.html)))` mounted before
`fallback_service`. A deep link and a cold start therefore cannot drift, which matters because the
sign-in return target *is* a deep link: `oauth::start_login(..., next)` stashes
`dashboard_target`, the callback lands on `/guild/123/tickets/`, and `pages/Guild.tsx` bounces an
unauthenticated visitor to `/login/?next=…` rather than losing the page. `verify-panel.sh` asserts
the shell, and asserts `/servers/`, `/moderation/`, `/tickets/`, `/theme.css` and `/theme-boot.js`
are all 404 - a route table that grows back fails the run.

**Fifteen pages per server**, all of them short-headed (`PageHeader` + `Panel` + `Table`, hints that
are one line of fact): Overview (what the bot sees, right now), Links & invites, Anti-spam, Text
filter, Moderation, Tickets, Stickies, Anti-raid, Welcome & roles, Levels, Logging, Commands, Bot
profile, Server settings, Audit. `lib/nav.ts` is the one table the rail, the router and the
per-page document list are built from - which is why the dead `accountPages` export (still pointing
at `/servers/`) got deleted rather than fixed.

**No toggle that nothing reads**, which is the rule that decided this round's feature list. The
local gates went into the pipeline, not into a settings drawer: `gate::local_guard` is a pure
function - `(content, &LinkGuardConfig, &AntiSpamConfig, seen) -> Option<(GuardHit, GuardAction,
u32)>` - that runs before the paid provider, so invite/link blocking, flood, mention, emoji and
caps limits cost nothing and answer with the matched text in the reason. Six tests cover it
(allowlist by host suffix, invites as their own category, flood firing only above `max_messages`,
the shape rules respecting `min_chars`, `DeleteTimeout` carrying its minutes, `reason_for` naming
rule + consequence + match). `AntiSpamConfig::default()` is off and `LinkGuardConfig::default()` is
on; both are asserted, because a test that assumes "default means off" is how a product ends up
deleting messages nobody configured it to delete. `AutoReply` stayed out of the panel for exactly
this reason: nothing consumes its document.

**Per-guild everything, including the bot itself.** `identity` (nick, bio, avatar hash) applies
through `PATCH /guilds/{id}/members/@me` - legal for a bot since 2025-09, and only on its own member
object - queued by `update_settings` as `identity.apply` and executed in `outbox.rs`, which splits
the staged data URI into `avatar`/`avatar_digest` so the bytes never enter `guild_config`.
`commands` reports what the guild has (`set_commands` → `guild_reports`) and re-registers with the
bulk-overwrite endpoint on change, never per boot: Discord allows 200 command creates per guild per
day, so "sync" is a button that queues one row, and the page says as-of when the list was learned.
`Profile.tsx` stages a preview image (`PUT identity/avatar` → digest), shows that a staged copy is
not yet on Discord, and offers Clear for both.

**Verification, this round.** `cargo test --workspace` → 289 passed / 0 failed (root 19, `tests/cli`
2, api 64, bot 51, core 153) with no warnings; `npm run build` → tsc silent, 4 routes prerendered,
`audit clean`; `sh scripts/verify-panel.sh` → 84 passed / 0 failed / 0 skipped. The shipped binary is
`bin/aerona-linux-x86_64`, 21,288,648 bytes, md5 `edbfe8c40fe1148a46904c802bbfa343`, static-pie, no
`PT_INTERP`. The same suite was then run **from the extracted archive** - `unzip`,
`python3 -m pytest tests/test_launcher.py -q` → 37 passed, and `sh scripts/verify-panel.sh` against
the extracted binary and the extracted `dist` → 84 passed / 0 failed - because the artifact someone
installs is the zip, not this working tree. Worth knowing for the next box: the sandbox had **no musl linker**, and
`scripts/build-static.sh` died inside `ring`'s build script with `cc-rs: failed to find tool
"musl-gcc"` until `sudo apt-get install -y musl-tools`; a glibc build is not a substitute, since the
whole point of the target is running on any panel image vintage. `AERONA_BUILD_OPTLEVEL=1
AERONA_BUILD_JOBS=1` is what fits the build in 2 GB of RAM (6 m 27 s cold, 30 s warm).

Also fixed: `ago()` used to return a bare "4 min" while nine call sites appended " ago", so every
report row in the panel read "just now ago" and "never ago" - the phrase is now the helper's, not the
caller's; the `for action in match f {…}` in `update_settings` became an `if let` (the release build
warned); `scripts/release-zip.py` no longer requires the deleted `dist/theme.css`/`theme-boot.js` and
instead refuses an archive whose `dist/assets` has no built stylesheet; `README.md`'s Appearance
section and `docs/DESIGN.md` were rewritten to describe the panel that exists.

`make-og.py` was also de-drifted: it had its own hex table with a comment asking whoever
changes the palette to change it there too, so it now parses the `.dark` block of `index.css`
(oklch → sRGB, alpha composited, a missing token is a hard exit) and counts the routes it
advertises out of `pub const ROUTES`; the card said "PAIRING CODE - no account, no password" two
rounds after pairing was deleted, which is what that parse is meant to make impossible.
`theme-color` for dark moved from `#09090b` to `#0a0a0a`, the hex `--background` actually converts
to, and `verify-panel.sh` asserts the pair so the browser chrome and the page cannot disagree.

**Now open:** `Feature::AutoReply` is a stored document with no reader - either the worker grows a
reply engine or the model and its migration go away. The `/guild/` prefix is declared twice (in
`APP_PREFIXES` and as an axum route) with nothing checking the two agree; a test over the real router
would be cheaper than the next round of drift. And the panel's guild pages are verified by `tsc`, the
prerender audit and curl - there is no browser-level run, so a component that renders but cannot
interact still passes.

## 13. The door: a 401 is a redirect, not a page

Reported from the outside: the root showed **"401 unauthorized"** to a browser that had never signed
in. Two things were wrong and only one of them was visible. The visible one: `/` is an app route, and
`AuthGate` did not exist - the bounce lived inside `pages/Guild.tsx`, so the server list fetched
`/api/v1/guilds`, got a 401, and rendered its generic "The API did not answer" card around the
API's own sentence. The invisible one: a session cookie that authenticates nothing (revoked, expired,
or re-minted after `AERONA_MASTER_KEY` changed) is `HttpOnly`, so **nothing in the browser can drop
it**, and every request would keep carrying it. That install 401s forever and the only fix is for
somebody to suspect "clear site data".

**The API now ends a cookie it refused.** `app.rs::gate` answers 401 that arrived *with* an
`aerona_session` cookie by appending `auth::cleared_cookie` - the same value logout sends, so
`Path=/`, `HttpOnly`, `SameSite=Lax` and `Max-Age=0` match how it was written, and the browser
actually deletes it rather than keeping a second copy. Three things it deliberately does not do: no
`Set-Cookie` on a 401 that carried nothing (an anonymous probe is not a session to end, and a
deletion header on every unauthenticated request is a free way to defeat any cache in front of the
API); none for an empty cookie, which was never a session; and none on a 403 or a 404, because
clearing there would sign an operator out of the panel for one server they may not read.
`a_cookie_that_authenticates_nothing_is_cleared_by_the_refusal` in `tests.rs` asserts all four cases
against the real router, and `verify-panel.sh` re-checks the header over HTTP.

**One gate, in the shell.** `AuthGate` in `App.tsx` wraps the router's view and owns the whole
decision, so no page has to remember it: a route marked `app` with a settled "no session" renders
**nothing** and replaces itself with `/login/`, carrying `?next=` for the URL the browser actually
wanted (`/` is left off, because that is where it goes anyway). It waits for `ready` rather than
guessing - hiding the view while `/me` is in flight would blank a signed-in page for a round trip and
strip the prerendered document before hydration. `pages/Guild.tsx`'s private copy of the guard is
deleted, which is the only reason the behaviour was uneven in the first place.

**The login page asks.** It was called "Authorize this browser", which is what a person reads as a
permission dialog for a device. It says **"Sign in to see your servers"** now, with one line about
what the hop does (no password; Discord says which servers you administer; nothing shows before
that), and when you arrived via a bounce it names the page you were sent away from - the same
`?next=` the callback reads, so the round trip returns you to `/guild/<id>/tickets/` rather than the
top.

**A session lost mid-visit ends the visit.** `request()` announces every 401 with an
`aerona:unauthorized` window event; `SessionProvider` re-reads `/me` when it hears one *and* had a
session, so a revoke is a sign-out - one toast, "Your session ended. Sign in again.", then the gate
bounces. It is guarded on having had a session because a signed-out browser's ordinary `/me` 401
would otherwise refresh itself forever. `fetchMe` also clears the module's CSRF secret when the
session is gone: a page that still believes it holds one fires writes the server answers with an
unexplainable 403. And `ApiError` no longer hands `unauthorized` to a component to paste into a
card - a 401's message is "Not signed in - sign in again.", because if any of this text survives
into a toast it has to read as the instruction it is.

**Verified:** `cargo test --workspace` → **290 passed / 0 failed** (root 19, `tests/cli` 2, api **65**,
bot 51, core 153) with no warnings; `npm run typecheck` → clean (note: `typescript` had gone missing
from `node_modules` mid-round, so the "tsc silent" claimed one step earlier was not actually true -
`npm run build` chains `typecheck` first and only runs if it passes, and after `npm install` both the
typecheck and the four-route build are real); `npm run build` → 4 routes, `audit clean`;
`sh scripts/verify-panel.sh` → **89 passed / 0 failed / 0 skipped** against the rebuilt binary
(`bin/aerona-linux-x86_64`, 21,288,648 bytes, md5 `dd7806cf1c6c30cc55aefccf2f3e2bfd`, static-pie, no
`PT_INTERP`). The README's "press Authorize with Discord" walkthrough and its per-crate test counts
were stale in the same way the guard was, and are now true.
## 14. The panel that would not mount: a tooltip without a provider, a CSP that ate the theme, and a shell

The report was one sentence plus a console: *"It loads but the server part and like configs and stuff
dont work. like the different tabs dont work."* Four separate defects, all of them invisible in the
panel's own UI code, which is why they arrived as one symptom. The guild page renders or it does not,
and everything else looked fine.

**A registry component that renders a tooltip must bring its provider.** `SidebarMenuButton` puts
every menu label in a `<Tooltip>`, and `tooltip.tsx` exports `TooltipProvider` - which nothing
rendered. Base UI does not degrade when a Tooltip has no provider; it throws (the minified build
reports it as `Base UI error #31`), so `SidebarProvider`'s subtree - every `/guild/<id>/…` page -
failed to mount. That is the whole "tabs don't work". The provider is mounted inside
`SidebarProvider` (`delay={0}`), not at the app root, because the requirement belongs to the vendored
component: a registry file that needs context and does not supply it cannot be used by an app that has
no idea it needed one. If another `components/ui/*` file is copied in and it uses `Tooltip`,
`Dialog`, or anything else with a provider, the same rule applies: read its `Provider` exports.

**`script-src 'self'` and an inline bootstrap are incompatible, and the failure is bigger than colour.**
The theme decision lived in an inline `<script>` in `index.html`; the policy the server puts on that
document refuses it (`Executing inline script violates the following Content Security Policy directive
'script-src 'self''`), so the class on `<html>` never landed and React then hydrated a prerendered
document whose colours it disagreed with - a CSP refusal and a hydration error from one character of
markup. The bootstrap is now `public/theme-boot.js`, linked first in the head. A file rather than a
`hash` added to the CSP, because a hash has to be maintained in two places and drifts silently (every
edit to the inline block invalidates it, and the failure is again invisible); the nonce machinery
`csp_header` already has cannot help a static document, whose CSP is computed per response.
`prerender.mjs`'s audit is inverted to match: it used to *require* the inline snippet and *fail* on
`theme-boot.js`, so the same edit that moved the file also rewrote the rule - it now asserts the link
exists, that it precedes the stylesheet (the order is the whole point: after it, the paint has happened),
that the built file is present and reads `aerona.theme`, and that `<head>` holds no executable inline
script for the policy to refuse. JSON-LD is exempt from that last rule: a data block is not executed,
and failing it would teach the next person to delete structured data instead of fixing the check.

**`/guild/{*rest}` was serving the wrong document.** It pointed at `dist/index.html`, which is not a
shell - it is the *prerendered server list*, body included. So a deep link hydrated guild markup against
a list of guilds: React noticed, discarded the HTML and mounted, which is why the page worked at all
while logging `Hydration failed … #418` on every visit. The build now emits `dist/shell.html`: the same
template with its slots unfilled, hence `#root` present, `#main` absent, the app's own
`<script type="module">` and stylesheet intact. `main.tsx` already chooses between `hydrateRoot` and
`createRoot` by looking for `#main`, so this is the path it was written for, not a compromise.
`app.rs` serves the shell for the prefix and falls back to `index.html` when a build predates it, so an
old `dist/` degrades to the previous behaviour instead of 404ing. The file is reachable by name - it is
a file in the document root, and `static_headers` only refuses hidden paths and misses - which is
harmless here because it is empty by construction and `noindex` like everything else in this build.

**Two console messages were not bugs, and one was not ours.** `Permissions-Policy` named ten features
the platform has since retired (`document-domain` reverted, `sync-xhr`, `ambient-light-sensor`,
`battery`, `navigation-override`, the two `execution-while-*`), and each unknown name prints
`Unrecognized feature '…'` once per page load: a denied capability nobody can grant, at the price of a
red line in the operator's console. Trimmed to what still exists, with a core test that checks the list
against a list of *retired* names - asserting the header contains what the header contains is how the
bogus ones survived this long. `cross-origin-opener-policy` is ignored on a non-secure origin and the
browser says it is ignoring it; it is now emitted only when `base_url` is https, which the caller
already knows as `cookies_secure()` - hence `security_headers(nonce, secure)`. And `attribution-reporting`
plus `private-aggregation` from the pasted log appear nowhere in this repo (`strings bin/aerona-linux-x86_64`
finds the other eight and not those two), so they are a proxy in front of the install adding its own
directives. There is nothing here to fix, and now a test that says so in a comment instead of a bug
report each time someone reads their own console.

**`immutable` on everything was a deploy bug, not a performance tuning.** `cache_and_finish` gave
`public, max-age=31536000, immutable` to every successful response that was not HTML - which includes
`/favicon-48.png`, `/site.webmanifest`, and now `/theme-boot.js`, all of which keep their name when they
change. The fix is to limit the promise to content-hashed names (`is_fingerprinted`: `/assets/<stem>-<8+
alnum>.<ext>`, since vite's `[hash]` is base64-ish, and `logo-2.png` deliberately does not qualify), not to
append `?v=` strings to every reference in the document. A unit test lists real build output beside the
root files that must not be pinned.

**The mark.** `artwork/aerona.png` is the art; `python3 scripts/make-icons.py` derives all nine files in
`public/` from it (favicon 16/32/48, `apple-touch-icon` 180, 192 and 512 for the manifest, a 512
maskable, and two `aerona-mark*` inks for the panel header). Two bugs in that generator are worth
remembering because the output looked *fine*: taking alpha from raw luminance made the art's white field
the opaque shape (invert first), and treating "ink" as black made the glyph read washed out, because the
art's ink is dark grey - coverage is now scaled by `255 - <darkest grey>`. `brand.tsx` swaps the ink with
`dark:hidden`/`dark:block`, so the logo follows the same class that follows the theme; `favicon.svg` is
unreferenced. The manifest pointed `start_url` and `id` at `/app/`, a path the server 404s (and verify-panel
asserts it 404s), so an installed PWA would have opened a dead URL. `make-og.py` now pastes the real mark
instead of the drawn square that stood in for it, sized to the title's cap height and choosing its ink from
the luminance of the palette it already parsed out of `index.css`.

**Verified:** `cargo test --workspace` → **293 passed / 0 failed** (root 19, `tests/cli` 2, api **66**,
bot 51, core **155**) with no warnings; `cargo clippy --workspace --all-targets -- -D warnings` clean -
which first needed three pre-existing lints cleared, all of them one-liners (a dead `let free = … * 0`
and its `let _ = free` in `count_mentions`, a hand-written `Default for IdentityConfig` that was exactly
what the derive already gives an all-`Option` struct, and a `> 4_000_000 || < 512` that reads better as
`!(512..=4_000_000).contains(…)`), plus one `#[allow(clippy::too_many_arguments)]` carrying a comment in
`gate.rs`: `enforce_guard` has nine arguments and the honest fix is a params struct, but that changes how
moderation is called and this round's subject is the panel. `cargo fmt --check` reported 49 diffs in files
this round did not touch - import order and signature wrapping under rustfmt 1.99 - so it was *not* claimed
as green here; every block this round added was clean on its own, and a repo-wide `cargo fmt` was a separate,
mechanical commit. `npm run build` (typecheck → client → ssr → prerender) → 4 routes + 1 shell, `audit
clean`; `python3 -m unittest discover -s tests -t .` → 37 launcher tests OK; `python3 scripts/make-og.py`
→ 18 measured draws, no overflow. `sh scripts/verify-panel.sh` → **122 passed / 0 failed / 0 skipped**, run
twice: once in this tree and once in the tree extracted from the delivered zip (the zip's own required-file
list now includes `dashboard/dist/shell.html` and `dashboard/dist/theme-boot.js`, so an archive built
before this round's fix is refused rather than shipped). Against `bin/aerona-linux-x86_64` rebuilt by
`sh scripts/build-static.sh`: 20,801,224 bytes, static-pie, no `PT_INTERP`, `aerona 1.0.0`.

**Left open, deliberately, in order of bite:** `main.py`'s "was the dashboard built?" probe is
`static_dir/index.html` alone, so a tree that predates `shell.html` looks built and the deep-link
hydration mismatch comes back with no clue attached; it should ask for `shell.html` and
`theme-boot.js` too, with a launcher test per missing file (the release script already refuses both).
`enforce_guard` in `aerona-bot/src/gate.rs` takes nine arguments and is allowed to for one more round;
one call site means a params struct is cheap whenever the guard next grows. And `cargo fmt --check` is
not clean repo-wide under rustfmt 1.99 - 49 diffs, mechanical (import order, wrapped signatures) - which
wants one dedicated commit rather than being smuggled into a functional round, because it touches every
crate and would bury the next review.

## 15. The caption that was a group label: what #31 actually said, and two gates to catch the class

The previous round's post-mortem was wrong about the headline bug, and the way it was wrong is worth
writing down. The console said `Base UI error #31` in a production build, which is a *code*, not a
message; instead of decoding it, I matched it to the one provider-shaped thing I could see missing
(`TooltipProvider`) and shipped a wrapper for it. Base UI's tooltip provider is
`React.createContext(undefined)`, read as `delay ?? providerDelay ?? OPEN_DELAY`
(`tooltip/trigger/TooltipTrigger.js:102`), so a tooltip without a provider is not an error at all -
the change fixed nothing, and the comment justifying it was a false claim about another library. It is
reverted here, comment and all. The real message, supplied in the follow-up report, was
`MenuGroupContext is missing. Menu group parts must be used within <Menu.Group> or <Menu.RadioGroup>.`

**The bug, in one sentence.** `components/ui/dropdown-menu.tsx` mapped `DropdownMenuLabel` onto
`MenuPrimitive.GroupLabel`, that primitive reads the context of a `Menu.Group` and throws without one,
and `pages/Guild.tsx` renders its caption directly inside `DropdownMenuContent` - which is what a menu
label *is* in this API - so `GuildSwitcher` threw the moment it was opened. `Menu.Portal` returns
`null` while the popup is closed (`shouldRender = mounted || keepMounted`), which is why nothing
appeared wrong until a click: the throw happened inside the page, React unmounted it, and the sidebar
that holds the navigation went with it. One defect, both reported symptoms - "the tabs don't work" and
"clicking the server thing".

**Fixed in the component, not at the call site.** A caption is a `div`: no context, nothing to be
missing, and - the part that makes the mapping wrong even when it survives - Base UI's `GroupLabel`
renders `aria-hidden: true`, because its job is to be the `aria-labelledby` target of a group, so a
label that should be read is exactly what it hides. A group-scoped `DropdownMenuGroupLabel` is exported
beside it for menus that really do group their items. `SelectLabel` had the identical mapping and was a
crash waiting for the first grouped option list; it is now a plain element with `SelectGroupLabel` next
to it. No page changed: the fix is in the file that was wrong.

**Two gates, because `tsc` cannot see this class.** `npm run build`'s audit now includes a rule over
`src/components/ui/*.tsx`: an exported wrapper named `…Label` that is not `…GroupLabel` must not render
`<X.GroupLabel>`. Writing it taught a lesson of its own - the first version's body regex ended at
`\n}`, and these functions open with a props annotation whose continuation line begins with `}`
(`}: MenuPrimitive.GroupLabel.Props & {`), so the rule read a type and never the element: it passed a
build it had been written to fail. It is anchored at `\n}$` now, and it was tested by putting the bug
back and watching the build refuse. The second gate is `scripts/audit-overlays.mjs`
(`npm run audit:overlays`, in the build chain): it compiles `scripts/overlay-fixture.tsx`, which renders
every overlay the panel opens the way the pages nest it, and asserts a marker string in the output for
each part a server render reaches. Its own honesty clause is load-bearing - Base UI mounts a popup
inside an effect, `renderToStaticMarkup` runs no effects, so the nine popup *bodies* are reported as
compiled-but-unasserted rather than as passes; a harness that renders nothing and prints twelve OK is
worse than none, because it reads as evidence. The fixture is sabotaged-tested too: move
`ProgressLabel` outside its root and the run fails with *rendered without "Strike 1 of 3" - the
assertion is not being reached, which is not a pass*.

**And when it does break, it should say so.** `main.tsx` now wraps the tree in
`components/error-boundary.tsx`: the message, the stack in a `<details>`, "Aerona is still running -
only this view broke", and a retry. It is deliberately built out of nothing but JSX and utility classes,
because the event being handled is a component crashing; a card assembled from `components/ui` would be
a white screen with better intentions. That boundary is what turns "the tabs don't work, nothing in the
UI" into one sentence the operator can paste.

**Verified:** `npm run build` → typecheck clean, 4 routes + the shell, `audit clean`, `overlay audit
clean` (3 asserted, 9 popup cases compiled); `sh scripts/verify-panel.sh` → **122 passed / 0 failed /
0 skipped**. No Rust changed this round, so the shipped binary is the same artifact as before
(`bin/aerona-linux-x86_64`, 20,801,224 bytes, md5 `f25e379689c3b40b4bc48214224d74b5`) and
`cargo test --workspace` remains 293/0; what changed is `dashboard/`, which is why the archive is
rebuilt. Still open, now with a name: a jsdom environment with `ResizeObserver`/`matchMedia` stubs would
let the fixture assert the popup bodies too, which is where this bug lived.

## 16. The tabs that only moved the URL, and the 83 keys the API had never heard of

The switcher opened, so the next report was about what was behind it: clicking *Stickies*,
*Anti-raid* or *Moderation* set the URL and did nothing else - no request, no console line, no
change. Both halves of that sentence were the diagnosis, and both pointed away from the usual
suspects: nothing had thrown, and nothing had been *asked* to fetch.

**A segment that was matched but never captured.** The route table used to build one pattern per
nav row - `/guild/:guild/stickies/`, `/guild/:guild/raid/`, and so on - generated from `lib/nav.ts`
so a page could not exist in the sidebar without being routable. `pages/Guild.tsx` then reads
`params.page` and picks `VIEWS[page]`. Those two facts are compatible only if the segment arrives
as a *param*, and a literal segment in a pattern matches without producing one: `params` held
`guild` and nothing else, so `page` was `''` on every path, `VIEWS['']` is `Overview`, and the
guild pages were one view wearing fifteen URLs. No error because nothing was wrong with any
component; no request because the tab data comes from the same guild document that had already
been fetched. The fix is two patterns instead of fifteen - `/guild/:guild/` and
`/guild/:guild/:page/` - with `RouteDef.check`, which is how the nav table keeps its authority: a
pattern can match `/guild/1234/notapage/`, and `pageFor` decides that the product has no such page,
so the path falls through to the 404 the API already serves instead of painting an empty pane with
a 200. `meta` now takes the params it needs, which is why a tab change also changes the title it
had no way to know about.

**Proved before it was fixed, again.** The lesson from last round was that a guess about a minified
error costs a whole cycle, so this one started as a failing assertion rather than a patch: a
`check` case in `scripts/overlay-fixture.tsx` calls the real `matchRoute` for every row in
`lib/nav.ts` and returns the sentence explaining the disagreement. Against the unfixed tree it
printed fourteen lines of `/guild/1234/raid/ yields page="" but the row's segment is "raid" - the
view would not change`, and those same lines are what "passes" now means. A `check` case is not
markup and is not a grep: it is the two modules that have to agree, executed.

**"Make the features work" was a spelling problem across a language boundary.** The settings
documents are Rust structs, serialized with serde in snake_case, and `default_doc` in the API does
double duty as the accepted-key list so that an unknown key is a 400 rather than a 200 with the
old number still in the row. The panel wrote camelCase - 83 keys across ten pages, including a raw
`patchSettings(id, 'commands', { disabledCommands: … })` that no `FieldDef` list would ever have
shown. Every save on those pages was refused, which is what an operator experiences as "this
feature does nothing and I can't configure it". So the panel's keys are the model's keys now, and
`npm run build` cross-checks the two trees: a key the struct does not have fails the build, naming
the snake_case spelling it probably meant; a struct field with no control anywhere fails too,
unless it is exempted *with a reason* (`avatar` is staged through its own endpoint so a megabyte of
base64 never enters `config_history`). `tickets` had two fields nobody could reach -
`show_claim`, `show_close` - and they are switches on the page now. The check runs in the panel
build rather than in `cargo test` because the panel build is the thing that cannot ship a bad
document, and it refuses to shrug when `models.rs` is missing: an audit that silently skips is how
last round's harness reported twelve passes while the bug was in the tree.

**One icon set, one glyph per thing.** `@phosphor-icons/react` is gone; lucide is what the vendored
shadcn primitives already assume, which is the part the diff cannot show - the primitives size
`[&_svg:not([class*='size-'])]` to 16 px, so swapping libraries needed no per-site sizing and no
`size` props were added. `lib/nav.ts` rows now own the page's glyph, `SettingsForm` takes a card
`icon` and a `tone` (`warn` means *this changes what happens to a member*, and it is tinted so it
cannot be mistaken for a preference), and `PageHeader` reads the nav row rather than being told.
The build holds the line in three places: no two rows share a glyph, no config card is without one,
no page repeats one. Fifteen rows used to share three icons, nine of them a shield.

**The welcomer, which existed and lied.** The greeting was unreachable behind the routing bug, and
once reachable it had two claims the code did not keep: `welcome_direct_dm` was a switch with no
implementation, and the channel hint promised the server's system channel while the handler needed
the picker to be filled. There was a third, worse in the eye of the person who sees it: the shipped
default greeting is `member #{count}` and nothing substituted `{count}`, so every new member was
welcomed by a brace. The join path now sends a card (title, body, colour, avatar thumbnail, all
optional - an empty title *and* body send plain text, so this is a choice and not a migration),
DMs when asked, unpublishes after a delay when asked, skips bots on both join and leave, falls back
to the system channel like the hint says, and treats a channel it cannot post to as a courtesy
denied rather than a failed join - the roles and the audit row do not depend on it. Tokens moved
into `GreetingFacts`, shared by both directions, because the goodbye knew two of the six the
greeting knew; `Welcome.tsx` renders a preview with the unsaved draft, lists the tokens it
advertises, and `prerender.mjs` fails the build if either side names a placeholder the other does
not know. Three bot tests cover the token list, the "no card unless asked" rule and the default
message's placeholders.

**What went wrong on the way, because that is the useful part.** An attempt to tidy indentation
with a regex over JSX deleted real characters from a dozen files, and a follow-up "repair" - a
bracket-matching pass - deleted more, because it did not know that Rust strings and comments hold
unbalanced braces. Recovery came from the artifact: `src/pages` was restored from the zip shipped
earlier in this session, and every change was replayed from a script under `/tmp` instead of
re-typed. Two rules came out of it. Do not reformat with a pattern you cannot diff against, and
when a home-made checker reports a number, run it on a *known-good* file first - it said the
original `events.rs` was already `-4`, which was the only clue that the `-6` in my edited file was
two off rather than six. The real oracle was `cargo check`, two commands away the whole time. It
also caught `handle_leave` having been swallowed by an over-wide splice, which is the failure mode
of "replace from here to there" in a file with no version control under it.

**Verified for this artifact:** `npm run build` → typecheck clean, 4 routes + the app shell, `audit
clean` (prerender contract, vendored caption rule, guild-page icon and view rules, settings-document
cross-check), `overlay audit clean` (4 asserted, 9 popup bodies named as out of SSR's reach). Each
new rule was sabotage-tested after it was written: putting a camelCase key back produced
`key 'welcomeIgnoreBots' is not a field any of its features has, but 'welcome_ignore_bots' is
(autorole/welcome) - the API will 400 the save`, and duplicating a rail glyph produced `nav.ts: Pin
is used by more than one page - a glyph shared in a rail marks nothing`. `cargo test --workspace`
→ **296 passed / 0 failed** (core 155, api 66, bot 54, root 19, cli 2); `cargo clippy --workspace
--all-targets -- -D warnings` clean, which is how `0x5865_f2` became `0x58_65_F2`.
`sh scripts/verify-panel.sh` → 127 assertions over the built panel and the running binary,
including three new ones that read the *shipped bundle* for the `:page` route and the snake_case
settings keys, because a build that passed on this machine is not the artifact.

**Still open, on purpose:** jsdom so popup bodies can be asserted; `main.py`'s `needs_build`
checking `shell.html` as well as `index.html`; `enforce_guard`'s nine arguments. (§17 took the
`cargo fmt` diffs.)

---

## 17. The read that returns undefined, and the reformat that invalidated the binary

The keys audit only looked at what a page *writes*, which left the other half of the same mistake
unpoliced: a page that *reads* `settings('sticky').cooldownSecs`. A wrong write key is a 400, so it
surfaces the first time anyone presses Save. A wrong read key is `undefined`, and `undefined` renders
as a tile that says "off" forever - quietly, on the one screen whose job is to tell you whether the
bot is doing anything. So `scripts/prerender.mjs` now scans every file under `src/{pages,components,
lib,hooks}` for the camelCase spelling of any multi-word field of any settings struct.

Two things had to be true before that rule could be honest:

- **Single-word fields are exempt by construction.** `enabled` is the camelCase spelling of `enabled`
  and also of half the UI's props; requiring it to be spelled `enabled` proves nothing. The rule only
  fires on names that *changed* shape, which is where the bug lives.
- **Half the hits were correct.** The first run flagged `strikeThreshold` and `escalateAction` in
  `lib/api.ts`, `channelId`, and `avatarHash` in `Profile.tsx`. All four are keys the server itself
  emits - `routes/guild.rs:428`, `:429`, `:210`, `db/automation.rs:563` - because the hand-built
  responses (overview, report, identity row) are camelCase and only the *settings documents* are the
  structs' snake_case. The rule therefore reads the crates to learn which names the API emits, and
  exempts exactly those. A checker that has to be told what a correct answer looks like is only worth
  having if it can still be shown being wrong: the probe `data?.settings?.sticky.cooldownSecs` fails
  the build, and deleting the exemption flags four names, in two files that ship fine.

`README.md` grew a *Developing the panel* section for the thing every round tripped over: the dev
proxy is one line in `dashboard/vite.config.ts` (`/api` → `127.0.0.1:8790`), the panel has no API base
to configure at all - `src/lib/api.ts` builds paths against the page's own origin - so a preview that
cannot reach its origin shows the offline states by design rather than guessing at `localhost`, and
`AERONA_BASE_URL` is the deployed side of the same decision. The test counts there and above the
layout table now read 296 (155 core / 66 api / 54 bot / 19 aerona / 2 cli), and the section documents
`scripts/verify-panel.sh` as the one check that reaches the wire.

`cargo fmt --all` was finally run, and it changed 12 files. That is a whitespace-only diff and a
formatting check is cheap, but it is not a reason to ship the old artifact: the release binary in
`bin/` had been compiled from the *pre-format* sources, so "fmt clean, tests green, here is the
binary" would have been three true statements about two different trees. The binary was rebuilt, the
panel harness re-run against it (127/0/0), the zip cut from it, and the harness re-run against the
*extracted zip* - which is the only form of the release anyone else holds. Both runs green is the
claim; one of them is not.

---

## 18. The round where the panel stopped lying

Everything reported at once, because the reports were one sentence each and the cause was shared:
the panel had controls that did not reach the worker, and the worker had settings that did not reach
the behaviour. A locked server could not be unfrozen, `!mute` said nothing to the member, the picker
offered no channels, the transcript was a `.txt` that dropped the screenshots, and the log page read
the AI filter's table. The fix was mostly deletion.

- **Freeze and unfreeze.** `raid::apply` restored only what its loop had touched and gave up
  mid-list on a channel it did not manage, so a freeze outlived its own release. One path now for
  both directions, the verification level goes back to the number that was saved, and the expiry
  timer calls that same path instead of a second half-implementation. `stress.rs` pins the round
  trip.
- **Commands everyone can see.** A command is registered with `default_member_permissions` when it
  is staff-only and with nothing otherwise. Discord hides what a member cannot run, which is the
  only correct place for that rule; the panel's command table badges the gated ones `staff`.
- **Two seconds, not twenty.** Panel actions used to wait on the 20-second status tick because
  nothing else drained the queue. `queue_tick` runs every 2 seconds and owns the queue, the
  settings epoch, and the lockdown timer: a save is a save when the thing you pressed answers.
- **The picker that the bot never filled.** A channel list was written on the slow pass, so a
  fresh install saw nothing for an hour. Guilds with no report are marked dirty on `ready`, the
  drain is capped per tick so a bot in 400 servers cannot rate-limit itself, `POST
  /guilds/{guild}/picker-refresh` asks for one guild now, and every `ids` control has a Refresh
  button that waits on the report's own `reportedAt` rather than on a guess.
- **Channels look like channels.** `#general` in the list and on the chip, role colour and role
  icon beside the name, and a managed role that a human cannot be given is not offered at all.
- **Transcripts.** One renderer, in `aerona-core`. The API had grown a second copy with a different
  header, which is how the file you downloaded and the file the bot posted disagreed. HTML is the
  default, it inlines the images and the video and audio links, it marks staff notes, and it ships
  a CSP of `default-src 'none'` with no script element anywhere, so a transcript of a member typing
  `<script>` shows the text of it. `?format=` overrides per download; the content type is right, so
  the browser opens the file instead of printing it.
- **The member is told.** `notify::wants_dm` is the whole policy: any action that happens to a
  person carries the reason to that person, unless the guild closed DMs. `ModConfig.dm_reason` was
  deleted because it did nothing, and `LOG_EVENTS` lost `username_change` because the bot never
  emits it: a switch for an event that cannot happen is a switch that trains people to ignore the
  panel.
- **Logging without a master switch.** `LogConfig` has no `enabled` and never needed one: the
  channel is the switch, and the page says so in those words. The page's table now reads
  `GET /guilds/{guild}/log`, which is the same `log_events` rows the channel posts, in the keys
  `read_event` actually emits - the first version of this page read `row.channel_id` where the API
  returns `channelId`, which is the same class of bug as everything else in this round, caught by
  asserting on the row keys in `stress.rs`.
- **Tickets are editable.** Opening and closing cards take a title, a body and a colour, with the
  four tokens the renderer substitutes, and the page draws the card from those fields so a
  moderator sees what a member will.
- **Nothing is typed at an id.** A sticky is anchored to a message, so the form lists the messages
  the bot still holds text for, labelled with their own text and their channel's name. Selecting one
  fills the note in; leaving the note alone posts the message.

Copy went on a diet in the same pass: the longest hints on six pages were cut roughly in half, the
explanation of what a URL is left `Links.tsx` entirely, and every em dash in the tree is gone - 785
of them across sources, docs and scripts, replaced rather than reworded around, and
`verify-panel.sh` now fails a build that puts one back. It also fails a page that adds a switch for
an event `LOG_EVENTS` does not contain, which is the audit that keeps the logging page and the
logger from drifting again. Both counts are in the release gate below.

Verified in this environment: 313 tests (`164 core / 66 api / 62 bot / 19 aerona / 2 cli`), `cargo
clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`, the dashboard's
typecheck plus the prerender and overlay audits, and `scripts/verify-panel.sh` at 146 assertions,
0 failures, 0 skips - run once against the build tree and once against the extracted release zip,
which is the only form anyone else holds. `stress.rs` was sabotage-checked: making
`LogConfig::tracks` ignore the channel fails `no_channel_picked_means_nothing_is_recorded`, which
is the assertion that the whole round rests on.

Built with `AERONA_BUILD_CGU=64 AERONA_BUILD_OPTLEVEL=2` on a 2 GB box with no swap, against a musl
toolchain fetched into `~/.local/musl` (the script wants `musl-gcc` on `PATH`, and a game panel has
no musl packages to give it). 8m25s from a cold target. A two-character lint fix afterwards rebuilt
in 1m18s to the *same* md5, which is the proof that the fix moved no instructions: the binary is
22,071,200 bytes, `static-pie linked, stripped`, md5 `190fde68c6f35080748c180e25208b44`. The zip
holds that binary, `dashboard/dist`, and this tree in 222 entries; its own checksum is recorded
where the file is published, because a zip cannot contain the hash of itself.

**Still open, on purpose:** jsdom so popup bodies can be asserted; `main.py`'s `needs_build`
checking `shell.html`; `enforce_guard`'s nine arguments; and the honest limit of every number above
- nothing here has been run against a live guild, because a gateway session needs a bot token and a
server, which is the one thing this environment cannot have.

## 19. Nine reports, and the shape they all had

Nine issues, each one a sentence and a screenshot, and every one of them the same complaint about a
different page: a control that wrote something nothing read, or a behaviour nothing could reach. The
bot profile page answered a save with `identity has no setting(s): avatarHash`. The channel picker
listed forty `#general`s with no idea which was which. The log channel said "a message was edited"
and left the edit out. A sticky could only be a copy of a message, and the list of messages to copy
from was empty. The ticket page had switches for a card whose colour and buttons were fixed in code.
The server icon was missing from the switcher that lists servers. Half of every page was prose.

- **The profile that 400'd its own next save (RED-13).** `PUT /identity/avatar` wrote its digest into
  the settings document under `avatarHash`, a key the model does not have, and `normalize` refuses an
  unknown key - so the *next* save of anything on that page failed, quoting a spelling nobody had
  typed. Three earlier diagnoses of this issue had blamed a missing `rename_all`; the fix is to write
  `avatar_hash`, through the same validated path a nickname takes, so the audit history records one
  change and the image bytes stay out of it. The regression test stages an avatar and then saves a
  bio, which is the sequence that used to break, and asserts the document carries a hash and no
  base64.
- **Both halves of a change, in the embed (RED-15).** `logger::card` puts the sentence in the
  description and the two states in `Before` / `After` fields, fenced, with a fence *inside* the
  quoted text neutralised so a message that is itself a code block cannot end the fence early.
  `LogEntry::was_opt` / `now_opt` let a handler that does not know say nothing, because an unknown
  rendered as `""` reads as "it was empty". Every loggable event carries both halves now: a join and
  a leave carry the member's facts (ages, role count, nickname, whether it is a bot), a role change
  carries the two named role lists, a channel change carries kind, position, nsfw, slowmode, override
  count, topic and category, and a delete quotes the text that is gone instead of reporting that
  something was removed. The panel's table gives each half its own column, unclamped, with a jump
  link to the message.
- **A category where a category is the answer (RED-14, RED-16).** `FieldDef.source` grew a third
  value, and the ticket category field uses it, so the trigger says "Pick a category" and offers
  categories rather than text channels. Everywhere else a channel is picked, the list is grouped
  under its category's heading, with `Unfiled` for what has none: the categories are shown as
  headings, because selecting one is not what those fields mean.
- **A sticky you write, not one you copy (RED-19).** `StickyRecord` gained `embed_title`,
  `embed_body`, `embed_color`; `add_sticky` takes a draft with or without an anchor, and the row's
  unique index became partial (`WHERE message_id != 0`) so unanchored notes can multiply while an
  anchored one still updates in place. Creating one from the panel also queues `sticky.post`, so it
  appears now instead of after `everyMessages` lines of chatter. The Running table names the channel
  instead of printing a snowflake, and the message-copy picker is gone: a sticky no longer needs a
  message to keep fresh, and `!sticky` still copies one the bot holds text for.
- **A ticket type is a button, not a row (RED-18).** `ticket_panels` grew `color`, `subject`, `intro`;
  the API's panel routes became a CRUD the dashboard can drive (`panel.post` on add, `panel.redraw`
  on delete, `bind_panel` recording the message the worker chose); `ticket_intro` takes the panel and
  lets it win over the guild's colour and opening text; and `ticket_buttons` reads this server's
  wording for Close, Claim and Transcript, with blank stored labels meaning "the built-in words",
  which keeps a default recognisable as a default. The page edits all of it and shows the card it is
  describing, plus one button per type.
- **Twenty-five buttons is a board, found by writing the above.** `draw_panels` put every type in one
  action row. Discord takes five per row, so a sixth type turned the whole message into a request it
  refused, which from the dashboard looks like nothing happened - the exact report class this project
  keeps getting. The board now spreads its buttons over rows of five, and `add_panel` refuses a 26th
  with the reason, because a type nobody can click is worse than a write that says why.
- **Notes keep their lines.** The API read a sticky's text and an intro through `str_field`, whose
  `security::clean` folds a newline into a space: right for a label, and silently unformatting every
  multi-line note. `opt_block_field` checks that the value is a string and not absurd, and hands the
  shaping to `scrub_block`, which core was already going to call - a field cleaned twice is a field
  whose formatting was decided twice.
- **The banner that floated (RED-17).** The coloured strip at the top of a settings card was inside
  the header's padding, so it sat a few pixels below the corner it was meant to touch. It is two
  flush absolutely-positioned strips on the card now, which the card's own `overflow-hidden` clips to
  the corner radius for free.
- **The icon the switcher never got (RED-20).** `GuildSwitcher` had been rendering an `AvatarImage`
  since the day it was written; `Guild.tsx` built the `current` prop by hand and passed `icon: null`.
  The guild context now carries the icon from the same row the list uses, and one field in one context
  put every server's picture back in the rail.
- **The prose (RED-21).** Twenty-one `hint=` and `description=` props are gone, the ones that remain
  are a `?` badge that opens a tooltip instead of a paragraph under every field, and the card headers
  that used to explain their own tone now just carry the tone. `FieldHint` is exported and rendered by
  the overlay fixture, so the composition (a tooltip inside its own provider, on a `render`-prop
  button) is asserted by `npm run build`: five cases asserted, up from four.

Verified in this environment: 326 tests (`169 core / 70 api / 66 bot / 19 aerona / 2 cli`), `cargo
fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, the dashboard's typecheck
with both audits clean, and `scripts/verify-panel.sh` at 162 assertions, 0 failures, 0 skips - run
against the build tree and against the extracted release zip. That harness also ran the migration for
real: a v5 database came up through `applied migration version=6` and served the panel, which is the
only way the partial unique index on `sticky_messages` gets tested as an upgrade rather than a
rebuild. Two of the new API tests were written against the *broken* behaviour first - the folded
newline in `embed_body`, and the 400 on the profile save after an avatar upload - and both failed
until the product changed.

`verify-panel.sh` itself was a liar this round, and it took a second run to see it: `start` recorded
the pid of `setsid` rather than of the server, so `stop` killed nothing and the second phase found
its own port taken, which reads as "the server does not bind" - a product accusation, from a harness
bug. The pid is written from inside the new session now, and `stop` waits for the port to be free
before it lets the next phase start.

Built with `AERONA_BUILD_CGU=64 AERONA_BUILD_OPTLEVEL=2` on the same 2 GB box, which this time needed
the swap the packaging notes describe: without it, `serenity` was OOM-killed at 1.2 GB of RSS at
`opt-level` 2, 1 and 0, at 16, 64 and 256 codegen units, because most of that memory is rustc's
front end and no flag touches it. `fallocate -l 4G /swapfile && mkswap && swapon` is the fix, and an
OOM kill mid-build is not a build error; the first attempt at 8m25s died 1m40s in, in a way that
reads like a broken workspace rather than a small one. The shipped artifact is 22,271,912 bytes,
`static-pie linked, stripped`, md5 `49d36091e0be9e955493095a4bcb73e9` - rebuilt once more after
the last test-only edit to the *same* hash, which is the proof that those edits moved no
instructions.

**Still open, on purpose:** jsdom so popup bodies can be asserted; `main.py`'s `needs_build` not
knowing about `shell.html`; `enforce_guard`'s nine arguments; the blurple `ticket_reply` still hard
codes; the board's own title is not a setting (its text is `default_message`, which is also the
ticket card's opening line - one field, two cards, and fixing that means a new config key, not a
patch); and, as always, none of this has been run against a live guild, because that needs a bot
token and a server.

**Close-out.** The zip is in the Drive releases folder as `aerona-v1.0.0-linux-x86_64.zip`
(id `11CH3C8LOkf4jFEB3BBKMdam0y6N536Y6`), with its size, md5, binary md5, what it ships and the
gate numbers in the file's own `description`, which is where round 11 recorded them and where
anybody who finds the file will look. Round 11's zip is still there, renamed to
`aerona-v1.0.0-linux-x86_64 (round 11, superseded).zip`, because the folder's habit is to keep a
superseded cut rather than delete it. No `.md5` sibling went up: no other file in that folder has
one, and the hash is in the description instead.

All nine issues (RED-13 to RED-21) have a comment saying what shipped for that report and which
check proves it, and all nine are Done. One caution for the next round, learned the hard way here:
the working notes had four of the nine numbers crossed with each other (the stickies work is
RED-19, not RED-16; the ticket embed is RED-18, not RED-17; the banner strip is RED-17; the server
icon is RED-20), and the first draft comment went to the wrong issue before each title was
re-read. Quote the issue text, not the number, when writing the report back.

## 20. Five reports about what a save does

Five issues, one user, and the same shape in each: something was written down that nothing obeyed,
or something was obeyed that nothing showed. "remove the toggle for that", "when you save it sends it
there", "when configured white it doesnt show full white", "all gathered from SOMETHING", "edited
messages dont even get logged". Read together they are one complaint about one habit - a settings
document as a place where a value can sit without anything having to act on it.

- **Reposting is the behaviour, so the switch went (RED-22).** `StickyConfig::repost` is gone; a
  sticky sends a fresh copy and deletes the one it made before, because the alternative - a stale
  copy parked at the top of the channel - is the one thing a sticky exists to prevent. Deleting a
  field from a model is not deleting a key from every database, and `normalize` refuses an unknown
  key, so an install that had ever saved the stickies page would have answered a 400 forever with
  "sticky has no setting(s): repost" - RED-13's bug from the other side. The fix is in the same
  place the rule is: `StickyConfig::RETIRED_KEYS`, consulted by `retire_keys` inside `normalize`
  *before* the unknown-key scan, so the key is dropped, the save proceeds, and the re-serialised
  document never carries it again. Anything else unknown is still refused, which is what keeps a
  typo from looking like a saved setting (the API test asserts both halves).
- **Where the board goes, and going there (RED-23).** `TicketConfig::board_channel_id` is the ticket
  channel; `add_panel` falls back to it, so a type no longer has to repeat the channel it will be
  posted in, and a page with neither answers 400 with the sentence that says what to do. Saving the
  tickets document now queues a redraw through `follow_up` in `system.rs`, which is where the
  identity and commands saves already did their "and now tell the worker" - so pressing Save is what
  puts the buttons in the channel that was just chosen, not a setting a future pass may honour.
- **A colour, on Discord and in the browser (RED-24).** Two separate faults, one report. On Discord,
  `StickyRecord::has_embed` asked for a title or a body, so "write a line and set it white" posted
  the line with no embed and dropped the colour on the floor; a colour now counts, and
  `sticky_message` sends the card with a bar and no text if that is what was configured. On the
  website, the pages drew their own idea of an embed - a `border-s-4` div - which fills the whole
  left edge, while Discord tints four pixels: white looked loud in the panel and invisible in the
  channel. Both sketches are gone; the preview is the real renderer
  (`@discord-message-components/react`, whose stylesheet the build inlines), so a preview that stops
  matching Discord is a bug in this file rather than a feature request. `ticket_reply` was still
  hardcoded blurple and now reads `TicketConfig::embed_color`, the last card in the worker that
  ignored a configured colour. Two details worth keeping: the package ships no types, so the props
  this panel uses are written down in `src/types/discord-message-components.d.ts` from its README and
  its bundle (its embed takes `borderColor`/`embedTitle`, not the older `color`/`title`), and its peer
  range is React 17, which `dashboard/.npmrc` answers so `npm ci` does not stop before it reads the
  lockfile. `ColorField` did *not* gain a `hint` prop: `lib/settings-form.tsx` already imports
  `iconUrl` from `pages/guild/shared`, so a hint there would have turned a one-way edge into a
  cycle - the sentence sits on the fields' own `hint`, where the form already renders it.
- **Banner, and decorations gathered from something (RED-25).** `IdentityConfig` grew `banner_hash`,
  `avatar_decoration_id` and `banner_decoration_id`. A banner is the avatar's twin, so it is the
  avatar's route shape (`PUT`/`DELETE /identity/banner`, bytes staged in a single-use report, the
  document keeping only the digest) and the same `ImageCard` component on the page. A decoration has
  no catalogue a bot may read, so the answer to "gathered from SOMETHING" is what the worker can
  actually see: `tasks::decoration_rows` counts the `avatar_decoration_data` of the members in cache
  and reports it as `picker.decorations`, and the panel offers those, oldest-worn first, plus a
  field for an id typed in. What the account does not own, Discord refuses - and that refusal is the
  report's job, not a claim of success. The report is where the real fix sits: `nick` is a guild
  member's field while `avatar`, `bio`, `banner` and the decorations are the account's, and the old
  apply sent all of it to `GuildMemberMe`, so the fields that were never member facts never applied.
  `identity_plan` now returns one request per field, and the apply fires each one separately, because
  `PATCH /users/@me` answers a key it will not take by refusing the whole body - batched, a banner a
  bot token may not write took the avatar and the bio down with it. The report carries which fields
  landed, which were refused and what Discord said about each, `avatar-pending`/`banner-pending` are
  spent only when their own field landed, and the panel lists the refusals per field.
- **The log that said nothing (RED-26).** `handle_edit` asked the event for its guild and bailed when
  it had none - and `MESSAGE_UPDATE` documents that field as optional, so *no* edit was ever logged.
  Deletes carried a guild but quoted from a cache with gaps, which is why only the `before` half was
  missing. `guild_of` takes the payload's guild or the channel's, filtered by the managed set either
  way; the quote comes from the guild's own bounded window first and serenity's cache second; and
  `delete_entry` always writes a row, saying plainly when there was nothing to quote, because a log
  that only records what it could quote teaches nobody that a purge happened. Serenity deprecates
  `Cache::channel` in favour of a call that clones the whole guild's channel map on every event, so
  the one place that needs the cheap lookup is `#[allow(deprecated)]`, with the tradeoff written
  where somebody would otherwise "fix" it back into a per-event clone.
- **Stress testing, as asked.** Fourteen tests over five files, one dropped: three on the sticky
  colour and the log handlers in `events.rs`, four on the plan in `outbox.rs`, one on the decoration
  count in `tasks.rs`, two on the seams in `stress.rs` (a delete quoted from the cache through the
  card it renders, and a guild found through the channel), four through the real API router with a
  cookie jar in `tests.rs` (a staged banner leaving the profile savable, a decoration validated where
  every identity field is validated, a sticky document from before the switch went away still saving,
  and the ticket channel being where the board goes). Two of them were written against the shipped
  behaviour first and failed: the API's deco route stored a clear as `null`, which the model reads as
  "nobody has said anything", and the banner test asserted a key the model writes explicitly. Both
  assertions were wrong in a way worth keeping as a test, so the code moved and the sentence stayed.

Verified in this environment: 339 tests (`169 core / 74 api / 75 bot / 19 aerona / 2 cli`), `cargo
fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, the dashboard's typecheck
with both audits clean, and `scripts/verify-panel.sh` at 186 assertions, 0 failures, 0 skips -
against the build tree and against the extracted release zip.

**Still open, on purpose:** jsdom so popup bodies can be asserted; `main.py`'s `needs_build` not
knowing about `shell.html`; `enforce_guard`'s nine arguments; `local_guard`'s tuple; `cleared_cookie`
in `auth.rs`; the board's own title is not a setting (its text is `default_message`, which is also the
ticket card's opening line - one field, two cards, and separating them needs a new config key, not a
patch); `handle_bulk_delete` still reports a count and the author of the first id rather than one row
per message, because a purge of forty messages is forty rows of the same sentence; and, as always,
none of this has been run against a live guild, because that needs a bot token and a server - which is
why the profile page reports what Discord answered instead of summarising what was asked.

## 21. A cape notifier, and the source that does not exist

"Ping me when a cape comes out, with a 3d render of it and how to get it." One feature, four
promises, and the first promise is a research problem before it is a code problem: Minecraft does not
publish a cape list. `version_manifest_v2.json` leads to asset index `34.json` (5147 objects, zero
cape entries); the client jar carries the cape *model* and *layer* classes and no textures; the popular
`minecraft-assets` mirror has a README on `master`. Everything Mojang ships about capes is the code that
draws them. The one place that lists them, dates them and holds their artwork is minecraft.wiki, so that
is the source: `action=query&generator=categorymembers&gcmtitle=Category:Capes&gcmlimit=500&prop=info&inprop=url`,
one request, which hands over exactly the three facts a notifier needs per cape (page id, title,
`lastrevid`) plus the article URL that answers "how do I get it". `prop=images` will not ride on a
generator - it comes back empty - so the file list is a second request naming the pages explicitly, and
that is a wiki quirk rather than a design choice. Fetching needs a real `User-Agent` or it is refused.
What the second request answers was measured afterwards, and is worse than a quirk: nothing. The wiki
names a cape's files from an infobox template rather than transcluding them, so `prop=images` is empty
for 56 of the 60 pages, and the four that answer list the gallery noise around the article (Turtle Cape
opens with a disambiguation dot, a minecart and an open barrel).

- **The bug every check missed, and the one check that caught it.** The first version asked each page
  for its files and picked among them, which is what both the code and its fixtures assumed the API does.
  It does not, per the bullet above, so most announcements would have shipped as text and the feature's
  whole promise was the picture. 361 tests, a 219-assertion panel script and a clean dashboard build all
  stayed green, because every one of them was written against the same assumption - a fixture can only
  testify to what it was built from. The live smoke test found it (`cargo test -p aerona-bot --test
  live_cape_wire -- --ignored`): it failed with "0 draped, 0 portrait-only", and after the picker was
  changed to ask for names rather than lists it failed again with "0 existed" out of 145 titles asked -
  a second bug, since MediaWiki answers at most fifty titles and replies to a longer request with an
  *error object*, which read as an empty page. Both are fixed: `wanted_files` names the seven files the
  wiki's convention implies, `about_page` drops gallery noise before it inflates a request, `image_infos`
  reads a batch and skips `missing`, `fetch_json` refuses an error body instead of letting it become "no
  capes", and the request is chunked at forty. The live run ends "6 draped, 2 portrait-only, 2 with
  nothing usable" out of the ten newest pages, and both declines are right: `Cape` is a hub article,
  `Blueprint Cape` has only an elytra file. The hero frames it wrote were opened and looked at; a draped
  `File:Aurora Cape (texture).png` is a cape and not a UV sheet.

- **The render is drawn here, not borrowed.** The wiki has two kinds of picture and only one of them is
  the cape: a flat 64x32 texture sheet (which is what a "3d render" request means when the answer is a
  UV map) and a portrait render someone exported. So `cape.rs` drapes the texture over the game's own
  cape layout - `drape()` walks the six quads, `project()` splats them far-to-near with a lambert term
  and a 0.42 ambient floor - at four yaws `[-0.62, -0.22, 0.22, 0.62]`, and `HERO_FRAME` names which of
  the four a message carries. Measured, not guessed: only 3 of the 60 pages have a ` (texture) JE.png`,
  and only some have a ` (texture).png`, so the picker has a preference order for both halves and the
  render kind (`drape`, `wiki`, `none`) is stored per release. The portrait fallback has an aspect rule
  because the wiki lies about what a "render" is: `Navy Cape.png` is a 714x501, 428 KB screenshot, and a
  rule of "png under 400 KB and taller than wide by 6/5" keeps it out while keeping every 280x636 render
  in. The first renders came out sparse and were blamed on the geometry; the alpha histogram said the
  texture is 72% transparent and the vertical fit had `off_y` subtracted instead of added.
- **A cape is announced once, per server, ever.** `remember_cape` is an `INSERT ... ON CONFLICT (guild_id,
  slug) DO NOTHING` and only a row that was actually inserted is a candidate for posting, so two servers
  watching the same cape get two pings and one server never gets a second. The first pass for a fresh
  install *seeds*: every existing cape is recorded with `announced_at` already set and no artwork is
  fetched at all, because the alternative is sixty old capes arriving in a channel that was configured a
  minute ago. After that, a page whose `lastrevid` moved is refreshed silently - an edited article is not
  a new cape - and new ones trickle out at three per tick so a bulk release is minutes of news rather
  than one burst. A channel that refuses the message (deleted, or the bot lost permission) gets
  `attempts += 1` up to five and then is left alone, with the count printed on the panel: a feature that
  retries forever is a feature that hides an outage.
- **The fetch lives in the worker, and the panel cannot make it happen sooner than that.** The API gets
  three routes: the list (config, releases with the hero `frameUrl` injected, `lastCheck`/`nextCheck`, the
  source `status` and `error` sentence), `POST .../capes/check`, and `GET .../capes/{slug}/frame/{frame}`.
  The middle one does not fetch: it stamps `last_check = 0` and answers "the worker checks on its next
  pass, within a minute", and the stamp is written even when the source is down, so forty installs do not
  repoll a wiki every sixty seconds because an operator kept pressing a button. Frames are served out of
  SQLite as the exact bytes the worker stored, and a release announced without a picture answers 400
  saying so rather than 404, because the panel needs words. There is deliberately no long `Cache-Control`
  on that route: the security middleware rewrites it for credentialed responses anyway, and a half-applied
  promise is worse than none.
- **The panel shows what will be posted, not a mockup of it.** The settings page is the six fields this
  feature has (channel, role, interval 15-1440, render toggle, embed colour - the colour is masked on read
  *and* write like every other colour, and the interval is clamped rather than validated, since a bot
  that polls the wiki twice a day is a policy decision and not a typo), then a card that prints the
  source's own status line, then a `DiscordEmbedPreview` of the newest release whose picture turns when you
  click it. The click cycles the frames the *server* listed (`frames` on the row, url rewritten from its
  own `/frame/N` suffix) instead of counting from zero, so the panel cannot ask for an index that does not
  exist.

Verified in this environment: 366 tests (`192 core / 75 api / 78 bot / 21 aerona`) plus the live wire
check, `cargo fmt --all
--check`, `cargo clippy --workspace --all-targets` with nothing left to say, the dashboard build with both
audits clean, and `scripts/verify-panel.sh` at **219 assertions, 0 failures, 0 skips** - against the
shipped musl binary, which had to be rebuilt for that number to mean anything.

Three things that only came out on the first real run of the panel:

- The cape assertions were appended at the *end* of `verify-panel.sh`, which is after the final `stop`:
  every request answered `000`, and because `$D/body` is not cleared between requests the `has` checks
  then found the previous body and complained that the SPA HTML had no manifest in it. The script has
  four phases with a server and a file-only tail; an HTTP assertion belongs in a phase, not at the end,
  and a `000` is read out of `$D/err` before anything is touched.
- `is_fingerprinted` decided a year of immutable caching by requiring the digest to be all
  alphanumerics. Vite's and rolldown's hashes are url-safe base64, so `index-B0IWrOJ_.js` - the name of
  this build's entry chunk - silently lost its `max-age=31536000, immutable`, and the next build would
  have had it back if the hash happened to roll differently. The rule is now `-` and `_` are digest
  characters and the split is at the *first* hyphen, because a hash can end in one. The unit test caught
  it the moment the real asset names went in; the README sentence "hashed assets are `immutable`" had been
  true in the source and false on the wire for a release.
- A `has` needle can be satisfied by a comment. "the embed points at the file the same message uploads"
  looked for `attachment://` in `bot/src/capes.rs`, which the doc comment contains, so breaking the
  `format!("attachment://{filename}")` line left the assertion green. It now quotes the interpolation, and
  three sabotages were run against the new cape assertions: two bit (`gcmlimit`, `reqwest` in the API
  route), the third is what taught the lesson.

**Still open, on purpose:** nothing in the feature has been seen by a live guild - the announcement is
built against serenity 0.12.5 and unit-tested to the attachment name, but "the embed showed the picture"
needs Discord. There is no "send a test post" button, so checking a channel and a role means waiting for a
cape or pressing Check now and reading the status line. `prune_capes` keeps 200 rows per guild, which is
generous for a feature that ships one row per cape a fortnight and wrong for somebody who leaves it off for
a year. A wiki that changes its category or its image naming will not break the notifier, it will just
announce less and say so in `status_line` - which is the honest failure, but it is still a dependency on
somebody else's HTML. And the poll interval is a per-guild setting with no global ceiling, so an install
with two hundred servers and a fifteen-minute interval is two hundred requests a quarter hour to one wiki.

## 22. The console line that said nothing

One report, one line of text: `Unable to preload CSS for
http://th-bkk03.altare.global:25154/assets/shared-DSbYiHhO.css`. That is Vite's preload helper giving
up on a stylesheet the router wanted for a lazily loaded chunk, and it is a symptom with no diagnosis in
it - no status code, no reason, and on the server's side nothing at all, because a static miss was a
response like any other.

Everything I first assumed was wrong, and each was cheap to kill once it was written down as a test:

- *the archive is missing the file*: no - `assets/shared-DSbYiHhO.css` is 13,614 bytes in `dashboard/dist`,
  in the zip, and the audit that walks every asset name referenced by the built html finds all 24 present.
- *the file is metered and the panel 429'd it during a burst*: no, and this one is worth keeping in mind
  for the next report. 150 parallel GETs of the stylesheet: 150 x 200. The gate at `app.rs:290` meters the
  API router only; the static half is `ServeDir` on a `fallback_service` (app.rs:162-167), so assets never
  touch the token bucket. The same 300 requests against `/api/v1/manifest` return 85 x 200 and 215 x 429,
  which is the meter working as designed and proves the difference is real, not luck.
- *a cached 404 from before the deploy*: no - a miss answers 404 with `cache-control: no-cache`, so no
  intermediary may keep it.

What is left is that the file was not in the tree their server serves (a deploy that copied the documents
and not `assets/`, an unpack that skipped a file, a stale `AERONA_STATIC_DIR`), and that nothing in this
product could tell them so. Two changes, one to remove the class and one to make it speak:

- **One stylesheet, linked by the document.** `cssCodeSplit: false`, so the panel's css is
  `assets/style-<hash>.css` in a `<link>` in `index.html` and `shell.html`, fetched by the browser's own
  loader. The failure mode is gone rather than handled: no preload helper, no route that can come up
  half-painted, one less request per navigation. It cost 148 KB in the entry instead of 13 KB on demand,
  which for a panel that is one page and always opened to the same place is the right trade.
- **A miss in `assets/` says what it found.** `app.rs` now warns with the path and the root, because the
  only links to an asset name come from the bundle that produced it - a 404 there is a broken deploy, and
  that is exactly the thing an operator can fix in one command. The 404 body and headers stay as they
  were, so nothing that was right about the miss changed.
- **`main.py check` reads the bundle's own list.** It walks `index.html` and `shell.html` for
  `assets/<name>` and reports either "all 4 asset(s) the html names are present" or names the ones with no
  file behind them and says to copy `dashboard/dist` again. Verified by deleting files from a copy of the
  built tree: one missing stylesheet is named, then a missing router chunk and a missing stylesheet are
  both named.

Verified here: 366 tests plus 2 new launcher tests (39 there), `cargo fmt --all --check` and
`cargo clippy --workspace --all-targets` silent, the dashboard build with the prerender and overlay audits
clean, and `scripts/verify-panel.sh` at **234 assertions, 0 failures, 0 skips** - the new ones assert the
single stylesheet in the built tree, that no chunk reaches for another, that a missing asset is a 404 that
nothing may cache and that the warn fires on the live server. Sabotage-tested as a matter of course now:
flipping `cssCodeSplit` back makes "the build emits exactly one stylesheet" and "no chunk reaches for
another one" go red; commenting the option out (leaving the text in a `//`) made the *third* check stay
green, which is the tautology trap a second time - a needle satisfied by a comment about the option
rather than the option. It now counts only a line that *is* the statement, and then it bites.

**Still open:** the cause on that box is not known from here, only made visible: their `ls` of
`dashboard/dist/assets` and `python3 main.py check` will say which it is in a sentence. The other thing I
cannot fix from here is that a half-failed asset load stays quiet in a *browser* - the panel now paints
from one request, but a lost JS chunk would still surface as a broken page rather than a message.
