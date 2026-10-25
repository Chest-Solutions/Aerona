# How the panel is built

Not a style guide for its own sake: every rule here exists because breaking it made something in
the product lie, hang, or cost an operator their evening. The visual system is shadcn/ui on the
`base-nova` preset — components in `dashboard/src/components/ui/`, tokens in
`dashboard/src/index.css` — and there is no second design system in this repo to argue with.

## One surface

`/` is the server list. `/guild/<id>/<page>/` is a server's page. `/login/` and `/access/` are the
only other routes. There is no marketing site, no docs page and no `/app/` prefix, and a 200 from
any of those means a route table grew back.

Guild pages are client routes, so no file can match them: `aerona-api`'s static layer serves
`dist/index.html` for anything under `/guild/` (`APP_PREFIXES` in `routes/system.rs`), which is the
same thing that lets a sign-in come back to the page it left.

## Short copy, or none

A setting says what it does in one line, under the field. If a control needs a paragraph to be
understood, the control is wrong. This is why `AntiSpam` is three small forms rather than one
twelve-row table, and why `Links & invites` shows the allowlist as chips.

## A toggle that does nothing is a bug

Every switch in the panel writes a document that the worker reads on the next message, and every
list is something the worker learned. Nothing renders a control whose effect is "a number is stored
somewhere":

| Page | Writes | Read by |
| --- | --- | --- |
| Text filter, Links, Anti-spam | `ai_filter`, `link_guard`, `anti_spam` | `gate::evaluate` → `gate::local_guard` then the provider |
| Moderation, Tickets, Stickies, Anti-raid, Logging | one document each | `commands.rs`, `events.rs`, `tasks.rs` |
| Welcome & roles, Commands | the `autorole` document | `db::auto_config` |
| Bot profile | `identity` + a staged image | `outbox.rs` → Discord's Modify Current Member |

`autorole`, `welcome` and `commands` are three screens over one stored document
(`Feature::storage_key`), because the worker reads one row for join roles, the greeting and the
command list. Three rows would be three chances to disagree about whether the greeting is on.

## Discord is only ever reached from the worker

The API holds no bot token. A save that needs Discord writes a row in the outbox and returns
`{"queued": id, "poll": "/api/v1/outbox/<id>"}`; the worker performs it and reports the result. That
is why a slow Discord makes a page say "queued" instead of failing, and why restarting mid-apply
loses nothing.

Per-guild bot identity goes through `PATCH /guilds/{guild.id}/members/@me` with `nick`, `bio` and
`avatar` — legal for a bot as of 2025-09, and only for the bot's *own* member object in that server.
The avatar's bytes never enter a settings document (that document is copied whole into
`config_history` on every save); they are staged in `report:<guild>:avatar-pending`, consumed by the
apply that uses them, and deleted. The document keeps only `avatarHash`.

## Nothing in the panel is invented

Slash commands, channels and roles come from what the worker last wrote into `guild_reports`
(`GET /guilds/{id}/report`), so every list carries the time it was learned — "as of 4 min ago", never
a live count. A worker that has never reported makes the page say so and hides the buttons that
would do nothing.

## Verification is part of the build

`npm run build` runs `tsc` before bundling, then prerenders the four static routes and audits them:
one `<title>`, a `noindex` on a private panel, the inline theme script, both `theme-color` values, no
pinned dark class, no internal link that goes nowhere. `sh scripts/verify-panel.sh` then checks the
shipped binary: the deep-link shell, the sign-in bounce per failure reason, and that every removed
route stayed removed. On the Rust side, `cargo test --workspace` is the loop — including the pure
`gate::local_guard` and `outbox::identity_payload` tests, which is why the enforcement rules and the
clear-versus-omit semantics can be argued with in a test file rather than in a server.
