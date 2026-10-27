# Security

Aerona is self-hosted software that holds a Discord bot token and a database of moderation
records. Both are worth stealing, so the design assumes the host is reachable and tries to
make each single failure survivable and legible.

## Trust boundaries

| Boundary | What crosses it | How |
| --- | --- | --- |
| Browser → API | credentials, settings writes | same-origin cookie session or `Bearer` key; state-changing requests need both a valid CSRF token **and** an `Origin` that matches `AERONA_BASE_URL` |
| Script → API | API keys | `aer_live_…` keys, scope- and optionally guild-bound |
| API → SQLite | every query | parameterised statements only; no string-built SQL anywhere |
| API → worker | intended actions | `action_outbox` rows; the worker re-validates and never trusts the queue blindly |
| worker → Discord | moderation actions, messages | one token, held by one process, never exposed by any endpoint |
| API → Sightengine | message text, opt-in | HTTPS + basic auth; per-guild privacy flag redacts matched text before it leaves |

Anyone who can run `main.py` or read `AERONA_DB` is root on this install: the install key,
the pepper and the database live in the same directory tree. That is deliberate - Aerona is
a single-tenant tool and encryption at rest would need a key stored somewhere with the same
access. Treat the data directory as a secret.

## Credentials

* **API keys** are 256 bits of entropy, stored as `HMAC-SHA256(pepper, key)`. A database
  copy does not yield usable keys, and verification is a constant-time compare of a MAC,
  not a lookup by prefix. The plaintext is returned exactly once, at creation.
* **The pepper** is `AERONA_MASTER_KEY`, or - when that is unset - `HKDF-SHA256` over the
  bot token, so it is never a constant and never stored separately from a secret you
  already protect. A dashboard-only install (`AERONA_BOT=off`, no token) has neither, and
  deriving a pepper from the empty string would let anyone who read the source forge a
  session for it: first boot therefore writes 32 random bytes to `data/install.key`
  (mode `0600`) and uses that. Rotating the pepper invalidates every key and every
  session, which is the point; a *truncated* `install.key` is refused at boot rather than
  re-rolled, because a silently rotating pepper looks exactly like a broken login.
* **Sessions** are random 256-bit ids, `HttpOnly`, `SameSite=Lax`, `Secure` whenever the
  base URL is HTTPS, 12-hour expiry, and revoked in the database on logout - the cookie is
  a claim about a row, not a capability.
* **Sign-in is Discord, and only Discord.** There is no bootstrap credential. The dashboard
  used to accept a single-use code printed on the console, and that code is gone - from the
  router, the CLI and the page - because a second way in is a second way for someone else in,
  and it bypassed the only answer that matters: what Discord says about your permissions.
  `finish_login` mints a session for an account that holds Manage Server (or Administrator) on a
  guild this install has seen, and stamps that list into `sessions.guild_grants` (schema v4);
  `require_guild` checks every guild-scoped request against it, so a permission revoked on
  Discord ends the access at the next sign-in and `AERONA_SESSION_HOURS` decides how soon that
  is. Two consequences are deliberate rather than unfortunate: an install with no
  `AERONA_DISCORD_CLIENT_ID`/`_SECRET` has a dashboard nobody can enter, and an account that
  manages none of the bot's servers lands on `/login?error=no-access`. `POST /api/v1/pair` is
  not mounted at all, so an old portal entry or bookmark gets a `404` instead of a slower path
  to the same door; the `pairing_codes` table stays, because dropping a column means rebuilding
  the table on every existing install, and an unread table is cheaper than a migration that can
  fail halfway.
* **The bot token** is read once at start, wrapped in a `Secret` that zeroizes on drop, and
  is never echoed by a status endpoint, a log line, or an error. `Config::summary()` prints
  `set` / `not set` / `not used (AERONA_BOT=off)`. `main.py` redacts anything that looks
  like a token or key from child output before writing logs, and its own `--version` probe
  of a candidate binary passes no environment at all.
* **`aerona doctor` is the one command that sends the token somewhere new**: it makes a
  single authenticated `GET /users/@me` to `discord.com` over TLS so a wrong token can be
  named as the cause. Nothing else about the command touches the network, and no other
  component of Aerona sends a token anywhere except Discord itself.

## Requests

* Bodies are capped at 512 KiB before parsing, and parse failures never reach the database.
* Metering is per principal: a sustained allowance with a burst bucket, so a compromised
  key can hammer one endpoint at a bounded rate and nothing else. The pair and login routes
  get a much stricter bucket (0.1 rps) because they are where guessing happens.
* **Same-origin for state changes, with one narrow, documented exception.** A mutating
  request must present an `Origin` that matches `AERONA_BASE_URL` (or `AERONA_CORS_ORIGINS`)
  *and* the double-submit `X-Aerona-CSRF` header. When `AERONA_BASE_URL` was never set -
  which is what a fresh install on an IP address looks like - the request's own `Host` is
  accepted as its origin, but only when that host is an IP literal or `localhost`. The
  fallback exists because a browser cannot make a request whose `Host` is not the origin it
  dialled, and DNS rebinding (a hostile *name* that resolves to your box) is precisely the
  case an attacker controls, so names are excluded. Set `AERONA_BASE_URL` and the fallback
  is inert; that is the right setting for anything on a domain, and it is required for
  Discord's OAuth callback anyway.
* **Two endpoints are unauthenticated on purpose**: `/healthz` (liveness, no content) and
  `/status` (version, uptime, guild *count*, queue depth, database size, feature flags),
  because a supervisor or a container healthcheck has to be able to ask before a credential
  exists. Neither names a guild, a channel, a user or a secret, and `no-store` applies to
  both. `GET /api/v1/manifest` publishes the whole route table with its auth requirement so
  this list cannot drift silently.
* `Cache-Control: no-store` on every API response; `Vary` is set wherever the response
  depends on a cookie or an `Authorization` header, so a shared cache cannot serve one
  operator's data to another.
* Security headers (`CSP`, `X-Content-Type-Options`, `X-Frame-Options: DENY`,
  `Referrer-Policy: no-referrer`, HSTS with `preload`) are applied by one function to both
  the API and the static responses; the CSP has no `unsafe-eval` and no external script
  source.
* **Static serving refuses rather than guesses**: only `dashboard/dist` is a root, hidden
  path segments are 404 before any filesystem lookup, the path is resolved under that root
  and traversal (`..`, encoded or not) cannot escape it, and there is no SPA catch-all - an
  unknown path under `Accept: text/html` gets the prerendered 404 page at status 404, and
  every other unknown path gets the JSON error envelope. A soft 200 for a nonexistent URL
  is a phishing affordance, not a convenience.
* Error responses are the fixed envelope `{error:{code,message}}` with a safe message;
  internal detail goes to the log with the `X-Request-Id`, never to the client.

## The audit trail

Every state change writes a row: actor type and id, the guild, the action, an optional
target, the client IP, the JSON payload, and a hash chain over the sequence.
`verify_audit_chain` recomputes it, and the dashboard exposes the result - so "the record
was edited" is detectable, not merely assumed. Writes are best-effort *after* the action
succeeded (a failed audit write is logged loudly but does not undo a legitimate
moderation action); reads and verification are strict.

Retention is bounded (default 90 days) and an hourly pass applies it, so the audit table
cannot grow into a legal problem.

## The Discord side

* The worker is the only writer. Its effective power is whatever the guild granted the bot's
  role - Aerona never asks for `Administrator`, and the dashboard says which permissions a
  feature needs (`MODERATE_MEMBERS`, `MANAGE_MESSAGES`, `MANAGE_CHANNELS`,
  `MANAGE_ROLES`, `KICK`/`BAN` as configured).
* "Is this a moderator" is answered from the **guild's own configured role lists** (plus
  `Manage Messages` when the gateway supplied resolved permissions), not from a hardcoded
  permission bit. A role the operator listed is authority; an empty role list is no
  authority, never "everyone".
* Role hierarchy is checked before a kick/ban/timeout so a moderator cannot act on somebody
  at or above them; if that read fails, Aerona defers to Discord's own 403 rather than
  inventing a refusal.
* Self-moderation is refused outright (`/ban` on your own id is a footgun, not an action).
* Purges are queued, never executed inline, so Discord's 429 backoff applies; pinned
  messages survive; a minimum-age guard protects a bot's just-posted welcome or a
  moderator's announcement; and bulk deletion stops at the two-week Discord boundary
  instead of erroring.
* A temporary ban is a row with an expiry and the worker unbans it - a ban that outlives
  its stated duration is the most common way a moderation bot harms a user.
* The softban is a real ban followed by an unban (Discord's only way to clear a member's
  recent messages); it is recorded as `soft: true` so the audit trail is not misleading.
* Anti-raid auto-actions are gated on an *active* raid event: new-account, avatarless and
  username heuristics produce notes and alerts by default, and only kick or ban while a
  lockdown is in force. Mass-action limits and a cooldown bound what one trigger can do.
* A lockdown is slowmode plus (optionally) verification level, always with a scheduled
  release; the worker releases it even if nobody clicks, because a lockdown is a promise
  about time.

## The AI text filter

* Off unless a guild enables it. Message content is not read by the worker at all unless the
  `MESSAGE_CONTENT` privileged intent is on in both the config and the Discord portal.
* `AERONA_CONTENT_INTENT=false` (the default) means prefix commands and text scanning are
  disabled and the worker says so at startup rather than silently doing nothing.
* Local custom rules run before any network call, so a guild that only needs keyword rules
  sends nothing to a third party.
* Fail-open by default (`on_api_error: allow`) so a provider outage cannot mute a server;
  `block` is per-guild and explicit, for communities that would rather hold a message.
* `privacy_redact` strips the matched span from the payload sent and from the stored event;
  matched text is never put in a Discord-visible message.
* Verdicts are cached briefly per author and text hash to survive a copy-paste raid without
  a request per message - violations are deliberately not cached, so repeat offenders keep
  accumulating strikes.

## Known limits, stated plainly

* Rate limiting is in-process. Two API instances sharing one database need a shared limiter,
  which Aerona does not provide; run one.
* The audit chain detects edits by somebody without the pepper (they cannot forge a chain);
  an attacker holding both the database and the pepper can rewrite history. Filesystem
  access is the root of trust, not this chain.
* Session and ban revocation are eventual: Discord's own cache can delay a permission
  change, and a member removed from the moderator role may finish an in-flight action.
* Deleting a message is best effort - Discord refuses after two weeks, and another client
  can win the race. The audit row is the record of intent; the platform is not.
* The dashboard renders guild-supplied names through React's escaping and never injects
  HTML, but a guild name is still untrusted display text; the API caps and cleans it
  (`security::clean`, 120 characters) before storage.
* **Serving the dashboard over plain `http://` exposes what crosses it.** A game panel gives
  you one forwarded TCP port with nothing terminating TLS in front of it, so this is the
  normal shape of an install, not a mistake - and `main.py` says so in its startup notes
  rather than refusing to boot. What that leaves true: every API key creation response and the
  session cookie travel readable by anyone on the path, and the cookie carries no `Secure` flag
  until the origin is `https`. Take one of these instead for anything you care about:
  a Cloudflare Tunnel (outbound only, no inbound port, valid certificate), a proxy you control
  with TLS - then set `AERONA_BASE_URL=https://…` and `AERONA_TRUSTED_PROXY=true` - or
  `ssh -L PORT:127.0.0.1:PORT` and browse the loopback address.
* Absolute URLs the API hands a browser (the OAuth `redirect_uri`) are built from
  `AERONA_OAUTH_REDIRECT_URI` when it is set, otherwise from `AERONA_BASE_URL`, and only
  otherwise from the request's own `Host` - and that last step only when the host is an IP
  literal or `localhost`. It is never derived from a name the install was not configured
  with: a `Host` header an attacker controls, reflected into a `redirect_uri`, is how an
  authorization code ends up on their server. An install reached by an unconfigured name
  answers `302 /login?error=origin-unset` and says what to set.
* The `redirect_uri` sent to `/authorize` is the one stored with the PKCE `state` and replayed
  to the token endpoint, so the two halves of the exchange cannot be made to disagree by a hop
  in between. A state is single-use, bound to the address that started it, and tombstoned on
  reuse or expiry - a redeemed code cannot be spent a second time.
* `GET /api/v1/status` is unauthenticated and therefore reports the redirect configuration only
  as far as it is the operator's own: `redirectUri` is echoed for `pinned` and `base-url`, and is
  `null` when the value is derived per request, because echoing a `Host`-derived string would make
  this endpoint a way to read back what a chosen `Host` produced. `/login` renders the derived value
  from its own `window.location.origin`, which is the same string without asking the server.
* Message-scan coverage is bounded by design: messages over `max_chars` are truncated
  before classification, and the worker only sees events Discord sends it (edits and
  deletes are logged from the cache it maintains, which is bounded per guild).

## Reporting

Open an issue privately (or mail the maintainer named in `Cargo.toml`) with the version
from `aerona --version`, the relevant `X-Request-Id`, and no production data. Include a
repro against `python3 main.py run` on a fresh database if you can. Secrets in a report help
nobody: paste the config keys that matter, never their values.
