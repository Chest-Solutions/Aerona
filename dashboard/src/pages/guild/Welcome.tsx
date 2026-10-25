/**
 * First contact: a greeting, the card under it, the roles they get, and a goodbye instead where
 * you want that.
 *
 * Both forms write the same document on purpose — the worker reads one `autorole` row for join
 * roles, the welcome line and the goodbye line — so splitting them into three rows would be three
 * chances to disagree about whether the greeting is on.
 *
 * The preview below is not a toy. Every placeholder a page advertises is a promise about a string
 * the *bot* substitutes, and this one used to break it twice: the hint listed two tokens while the
 * bot knew five, and the shipped default greeting said `member #{count}` with no `{count}` to
 * replace — so a new member was welcomed to the server by a brace. `TOKENS` here and
 * `GreetingFacts::tokens` in `crates/aerona-bot/src/events.rs` are held equal by the settings audit
 * in `scripts/prerender.mjs` — a placeholder named in a hint that the bot does not substitute fails
 * `npm run build`, which is the only way a list that lives in two languages stays true.
 */
import { DoorOpen, Frame, Hand, UserPlus } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'

import { useGuild } from '../../lib/guild'
import { SettingsForm } from '../../lib/settings-form'
import { PageHeader } from './shared'

/** The placeholders the bot replaces, in the order they are worth knowing about. */
const TOKENS = [
  ['{user}', 'the mention, which pings them'],
  ['{tag}', 'their name, unclickable'],
  ['{server}', 'the server name'],
  ['{members}', 'the member count'],
  ['{count}', 'the same number, for “member #{count}”'],
  ['{id}', 'their snowflake'],
] as const

/** Sample values, so the preview shows shape rather than pretending to know the person. */
function preview(text: string, server: string) {
  const sample: Record<string, string> = {
    '{user}': '@newmember',
    '{tag}': 'newmember',
    '{server}': server || 'your server',
    '{members}': '1,204',
    '{count}': '1,204',
    '{id}': '123456789012345678',
  }
  return text.replace(/\{(user|tag|server|members|count|id)\}/g, (m) => sample[m] ?? m)
}

export default function Welcome() {
  const { settings, name } = useGuild()
  const auto = settings('welcome') as Record<string, unknown>
  const roles = Array.isArray(auto.autorole_ids) ? (auto.autorole_ids as string[]) : []
  const on = Boolean(auto.welcome_enabled)
  const message = String(auto.welcome_message ?? '')
  const cardTitle = String(auto.welcome_embed_title ?? '')
  const cardBody = String(auto.welcome_embed_description ?? '')
  const colour = Number(auto.welcome_embed_color ?? 0)
  const hex = `#${(colour >>> 0).toString(16).padStart(6, '0').slice(-6)}`
  const hasCard = cardTitle !== '' || cardBody !== ''

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Welcome & roles"
        hint="What a new member sees, and what they get, in the first minute."
        actions={
          <>
            {on ? <Badge>greeting on</Badge> : <Badge variant="outline">greeting off</Badge>}
            {auto.autoroles_enabled ? (
              <Badge variant="secondary">
                {roles.length || 'no'} role{roles.length === 1 ? '' : 's'}
              </Badge>
            ) : null}
          </>
        }
      />

      <div className="grid gap-5 xl:grid-cols-[minmax(0,1fr)_20rem]">
        <div className="grid gap-5">
          <SettingsForm
            feature="welcome"
            title="On join"
            description="Sent as they arrive. Nothing is said if the switch below is off, whatever is filled in."
            icon={Hand}
            tone="calm"
            fields={[
              { kind: 'switch', key: 'welcome_enabled', label: 'Send a welcome message' },
              {
                kind: 'ids',
                key: 'welcome_channel_id',
                label: 'In channel',
                source: 'channels',
                hint: 'Nothing picked falls back to the server’s system channel; a server with neither sends nothing.',
              },
              {
                kind: 'textarea',
                key: 'welcome_message',
                label: 'The message',
                hint: `Tokens: ${TOKENS.map(([t]) => t).join(' ')}. Clear it to send the card alone.`,
                rows: 3,
                max: 1000,
                placeholder: 'Welcome {user} to {server} — you are member #{count}.',
              },
              {
                kind: 'switch',
                key: 'welcome_ignore_bots',
                label: 'Skip bots',
                hint: 'Verification apps and other servers’ automations join too. Also quiets the goodbye.',
              },
              {
                kind: 'switch',
                key: 'welcome_direct_dm',
                label: 'Also DM them',
                hint: 'The one place a greeting is guaranteed to be read, and the one that reads as spam at 10 joins an hour.',
              },
              {
                kind: 'int',
                key: 'welcome_delete_after_secs',
                label: 'Unpublish after',
                hint: '0 leaves it up. The roles and the audit row are unaffected either way.',
                min: 0,
                max: 86_400,
                suffix: 'secs',
              },
            ]}
          />

          <SettingsForm
            feature="welcome"
            title="As a card"
            description="An embed under the line, for rules and a link to the intro channel. Both fields are optional: with neither filled, the greeting stays plain text."
            icon={Frame}
            fields={[
              {
                kind: 'text',
                key: 'welcome_embed_title',
                label: 'Title',
                hint: `Tokens: ${TOKENS.map(([t]) => t).join(' ')}.`,
                max: 256,
                placeholder: 'Glad you’re here, {tag}',
              },
              {
                kind: 'textarea',
                key: 'welcome_embed_description',
                label: 'Body',
                hint: 'Markdown, as Discord takes it: links, bold, and <#channel> mentions.',
                rows: 4,
                max: 2000,
                placeholder: 'Start in <#rules>, then say hello in <#general>.',
              },
              { kind: 'color', key: 'welcome_embed_color', label: 'Accent', hint: 'The bar down the left edge.' },
              {
                kind: 'switch',
                key: 'welcome_show_avatar',
                label: 'Thumbnail their avatar',
                hint: 'Drawn from the CDN URL the event already carries, so it costs no request.',
              },
            ]}
          />

          <div className="grid gap-5 sm:grid-cols-2">
            <SettingsForm
              feature="autorole"
              title="Roles they get"
              description="Assigned after the wait below, as they settle in."
              icon={UserPlus}
              fields={[
                { kind: 'switch', key: 'autoroles_enabled', label: 'Hand out roles on join' },
                {
                  kind: 'ids',
                  key: 'autorole_ids',
                  label: 'Which roles',
                  source: 'roles',
                  multiple: true,
                  hint: 'The bot cannot give a role above its own.',
                },
                {
                  kind: 'int',
                  key: 'autorole_delay_mins',
                  label: 'Wait',
                  hint: 'A minute or two stops a raid collecting pings.',
                  min: 0,
                  max: 1440,
                  suffix: 'mins',
                },
              ]}
            />

            <SettingsForm
              feature="welcome"
              title="On leave"
              description="Same tokens, quieter purpose."
              icon={DoorOpen}
              fields={[
                { kind: 'switch', key: 'goodbye_enabled', label: 'Say goodbye' },
                {
                  kind: 'ids',
                  key: 'goodbye_channel_id',
                  label: 'In channel',
                  source: 'channels',
                  hint: 'Falls back to the system channel.',
                },
                {
                  kind: 'textarea',
                  key: 'goodbye_message',
                  label: 'The message',
                  rows: 2,
                  max: 500,
                  placeholder: '{user} left. {members} now.',
                },
              ]}
            />
          </div>
        </div>

        <div className="grid content-start gap-3">
          <Card>
            <CardHeader>
              <CardTitle className="text-base">What they will see</CardTitle>
              <CardDescription className="text-sm">
                Unsaved changes included, with a made-up member. Nothing about the real join is guessed
                — only the tokens are filled in.
              </CardDescription>
            </CardHeader>
            <CardContent className="grid gap-3">
              {!on ? (
                <p className="rounded-lg border border-dashed p-3 text-sm text-muted-foreground">
                  The greeting is switched off, so this preview is what nobody gets.
                </p>
              ) : null}
              {message ? (
                <p className="text-sm">
                  <span className="text-muted-foreground">@newmember</span>{' '}
                  {preview(message, name)}
                </p>
              ) : null}
              {hasCard ? (
                <div className="flex overflow-hidden rounded-md border-l-4 bg-muted/40" style={{ borderLeftColor: hex }}>
                  <div className="grid gap-1 p-2.5">
                    {cardTitle ? <p className="text-sm font-semibold">{preview(cardTitle, name)}</p> : null}
                    {cardBody ? (
                      <p className="whitespace-pre-line text-xs leading-relaxed text-muted-foreground">
                        {preview(cardBody, name)}
                      </p>
                    ) : null}
                  </div>
                </div>
              ) : (
                <p className="text-xs text-muted-foreground">No card yet — the title and body are both empty.</p>
              )}
              {Number(auto.welcome_delete_after_secs) > 0 ? (
                <p className="text-xs text-muted-foreground">
                  Removed {(auto.welcome_delete_after_secs as number).toString()}s after it is posted.
                </p>
              ) : null}
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-base">Tokens</CardTitle>
            </CardHeader>
            <CardContent>
              <dl className="grid gap-1.5 text-xs">
                {TOKENS.map(([t, what]) => (
                  <div key={t} className="flex items-baseline gap-2">
                    <dt className="rounded bg-muted px-1 font-mono">{t}</dt>
                    <dd className="text-muted-foreground">{what}</dd>
                  </div>
                ))}
              </dl>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-base">What this needs from Discord</CardTitle>
            </CardHeader>
            <CardContent>
              <CardDescription className="text-sm">
                Manage Roles for the roles above, and Send Messages in whichever channels you picked —
                plus Embed Links if you want the card, because a greeting without its embed is the
                message and a bare colour bar you will not see. Nothing here is possible without them,
                and the bot keeps the attempts in its log rather than guess.
              </CardDescription>
            </CardContent>
          </Card>
        </div>
      </div>
    </div>
  )
}
