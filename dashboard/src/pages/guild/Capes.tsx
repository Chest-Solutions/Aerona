/**
 * Minecraft capes: a ping when one comes out.
 *
 * The source is `Category:Capes` on minecraft.wiki, which is where every cape - Mojang's own, the
 * tournament ones, the ones granted to a single person - is documented with a page, a date and an
 * article saying how it was earned. Mojang publishes no list: the asset index and the client jar
 * were both checked, and cape artwork is delivered per account rather than shipped in the game, so
 * a poll of the game's own files would find nothing forever.
 *
 * Two things on this page are deliberate, and both are about trust. Nothing here asks the wiki a
 * question: saving a setting writes a stamp and the worker does the fetching on its own clock, so
 * this panel opens and reads correctly while the wiki is down. And the first look at a server
 * records everything it finds without pinging - otherwise switching the feature on would mention a
 * role sixty times for capes that came out between 2011 and last Tuesday.
 */
import { useState } from 'react'
import { toast } from 'sonner'
import { Gem, RefreshCw } from 'lucide-react'

import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { TableCell } from '@/components/ui/table'

import { ApiError, capes, checkCapes, type CapeRow } from '../../lib/api'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { SettingsForm } from '../../lib/settings-form'
import { ago, DiscordEmbedPreview, PageHeader, Panel, Table } from './shared'

/** The frame index inside a url the API built, so the panel never has to know which angle is the
 *  hero one: it starts where the server pointed it and steps from there. */
function frameUrl(url: string, index: number): string {
  return url.replace(/\/frame\/\d+$/, `/frame/${index}`)
}
function heroIndex(url: string): number {
  const m = /\/frame\/(\d+)$/.exec(url)
  return m ? Number(m[1]) : 0
}

export default function Capes() {
  const { id, settings, reload } = useGuild()
  const list = useAsync((signal) => capes(id, signal), [id])
  const cfg = settings('capes') as Record<string, unknown>
  const rows = list.data?.releases ?? []
  const [busy, setBusy] = useState(false)
  /** Which angle each cape is being shown at, by slug. Turn the cape, in other words. */
  const [angle, setAngle] = useState<Record<string, number>>({})

  const enabled = Boolean(cfg.enabled ?? false)
  const channel = list.data?.channel ?? null
  const pingRole = list.data?.pingRole ?? null
  const status = list.data?.status ?? null
  const nextCheck = list.data?.nextCheck ?? null
  const error = list.data?.error ?? null

  const check = async () => {
    setBusy(true)
    try {
      const out = await checkCapes(id)
      toast.success('Asked the worker', { description: out.note })
      list.reload()
      reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  const turn = (row: CapeRow) => {
    if (!row.frameUrl || row.frames < 2) return
    const cur = angle[row.slug] ?? heroIndex(row.frameUrl)
    setAngle((a) => ({ ...a, [row.slug]: (cur + 1) % row.frames }))
  }

  const newest = rows[0]

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Minecraft capes"
        hint="A note in your channel when a cape comes out, with a render of it and the article that says how to get one."
        actions={
          <>
            <Badge variant={enabled ? 'default' : 'secondary'}>{enabled ? 'watching' : 'off'}</Badge>
            <Badge variant="secondary">{rows.length} seen</Badge>
            <Button size="sm" variant="outline" onClick={check} disabled={busy}>
              <RefreshCw className={busy ? 'size-3.5 animate-spin' : 'size-3.5'} />
              Check now
            </Button>
          </>
        }
      />

      <div className="grid gap-3 lg:grid-cols-[2fr_1fr]">
        <SettingsForm
          feature="capes"
          title="The notifier"
          icon={Gem}
          tone="calm"
          description="It reads one page - the wiki's category of capes - and tells you what is new to this server."
          fields={[
            {
              kind: 'switch',
              key: 'enabled',
              label: 'Watch for new capes',
              hint: 'Off means no poll at all: the list you already have stays, and nothing is fetched.',
            },
            {
              kind: 'ids',
              key: 'channel_id',
              label: 'Announce in',
              hint: 'Leave it empty to keep the list without posting anything. Capes found while it was empty are announced as soon as you pick a channel.',
              source: 'channels',
            },
            {
              kind: 'ids',
              key: 'ping_role_id',
              label: 'Ping this role',
              hint: 'Optional. With no role the card is posted quietly, which is what a server that only wants the news wants.',
              source: 'roles',
            },
            {
              kind: 'int',
              key: 'check_every_minutes',
              label: 'Look every',
              hint: 'The category changes a few times a year, and a pass is now one request for a guild that has seen everything, so a short interval is cheap. Below five minutes this is traffic for nothing, so the API raises it.',
              min: 5,
              max: 1440,
              suffix: 'minutes',
            },
            {
              kind: 'switch',
              key: 'show_render',
              label: 'Attach the render',
              hint: 'The picture is drawn from the texture file, or taken from the page when the wiki has no flat texture. Off sends the name and the link alone.',
            },
            { kind: 'color', key: 'embed_color', label: 'Card colour' },
          ]}
        />

        <div className="grid gap-3 content-start">
          <Card>
            <CardHeader>
              <CardTitle className="text-base">Where it looks</CardTitle>
              <CardDescription>minecraft.wiki, one request per pass.</CardDescription>
            </CardHeader>
            <CardContent className="grid gap-2 text-sm text-muted-foreground">
              <p>
                A cape is announced once per server, ever. A page being edited is not news, so the
                row updates and nobody is pinged again.
              </p>
              <p>
                The first look records every cape it finds in silence, because every one of them is
                new to a notifier that did not exist yesterday.
              </p>
              <p className="text-xs">
                {status ?? 'The worker has not reported a pass yet.'}
                {list.data?.lastCheck ? ` Last look ${ago(list.data.lastCheck)}.` : ''}
                {nextCheck ? ` Next in ${countdown(nextCheck)}.` : ''}
              </p>
              {error ? (
                <Alert variant="destructive">
                  <AlertTitle>The last look failed</AlertTitle>
                  <AlertDescription>{error}</AlertDescription>
                </Alert>
              ) : null}
              <p className="text-xs">
                Announce to{' '}
                {channel ? (channel.name ? `#${channel.name}` : channel.id) : 'nowhere yet'}
                {pingRole ? `, pinging ${pingRole.name ? `@${pingRole.name}` : pingRole.id}` : ', with no ping'}
                .
              </p>
            </CardContent>
          </Card>

          {newest ? (
            <Card>
              <CardHeader>
                <CardTitle className="text-base">The card it sends</CardTitle>
                <CardDescription>
                  {rows.length > 1 ? `The newest of ${rows.length} capes.` : 'The newest cape it has seen.'}
                </CardDescription>
              </CardHeader>
              <CardContent>
                <DiscordEmbedPreview
                  color={typeof cfg.embed_color === 'number' ? cfg.embed_color : null}
                  title={newest.name || newest.title}
                  body={
                    newest.render_kind === 'drape'
                      ? 'Draped from the texture file, so you can see the fold.'
                      : newest.render_kind === 'wiki'
                        ? "The wiki's own render of it."
                        : 'No artwork for this one on the wiki, so here is the page.'
                  }
                >
                  {newest.frameUrl && Boolean(cfg.show_render ?? true) ? (
                    <img
                      src={newest.frameUrl}
                      alt={newest.name || newest.title}
                      className="mt-2 max-h-40 rounded"
                    />
                  ) : null}
                </DiscordEmbedPreview>
              </CardContent>
            </Card>
          ) : null}
        </div>
      </div>

      <Panel
        title="What it has seen"
        hint="Newest first. Click a picture to turn the cape."
        actions={
          <Button size="sm" variant="ghost" onClick={() => list.reload()}>
            Reload
          </Button>
        }
      >
        {rows.length === 0 ? (
          <p className="px-3 py-6 text-sm text-muted-foreground">
            Nothing yet. {enabled
              ? 'The worker looks on the interval above - “Check now” asks it to look on its next pass, which is within a minute.'
              : 'Turn the notifier on and the first look records the whole category without pinging anybody.'}
          </p>
        ) : (
          <Table
            head={['Cape', 'Article', 'Found', 'Sent']}
            rows={rows}
            render={(row: CapeRow) => {
              const url = row.frameUrl
                ? frameUrl(row.frameUrl, angle[row.slug] ?? heroIndex(row.frameUrl))
                : null
              return (
                <>
                  <TableCell className="align-middle">
                    <div className="flex items-center gap-3">
                      {url ? (
                        <button
                          type="button"
                          onClick={() => turn(row)}
                          title={row.frames > 1 ? `Angle ${row.frames} per cape` : 'The wiki’s render'}
                          className="grid size-14 shrink-0 place-items-center overflow-hidden rounded bg-muted"
                        >
                          <img src={url} alt="" className="max-h-14" />
                        </button>
                      ) : (
                        <span className="grid size-14 shrink-0 place-items-center rounded bg-muted text-[10px] text-muted-foreground">
                          none
                        </span>
                      )}
                      <div className="grid">
                        <span className="text-sm font-medium">{row.name || row.title}</span>
                        <span className="text-xs text-muted-foreground">
                          {row.render_kind === 'drape'
                            ? `rendered here from ${row.texture_file || 'the texture'}`
                            : row.render_kind === 'wiki'
                              ? 'the wiki’s own render'
                              : 'no artwork on the page'}
                          {row.attempts > 0 && !row.announced_at ? ` · refused ${row.attempts}×` : ''}
                        </span>
                      </div>
                    </div>
                  </TableCell>
                  <TableCell className="align-middle">
                    {row.article_url ? (
                      <a
                        href={row.article_url}
                        target="_blank"
                        rel="noreferrer"
                        className="text-sm underline decoration-dotted underline-offset-2"
                      >
                        {row.title}
                      </a>
                    ) : (
                      <span className="text-sm text-muted-foreground">-</span>
                    )}
                  </TableCell>
                  <TableCell className="align-middle text-sm text-muted-foreground">
                    {ago(row.found_at)}
                  </TableCell>
                  <TableCell className="align-middle text-sm">
                    {row.announced_at ? (
                      <span className="text-muted-foreground">{ago(row.announced_at)}</span>
                    ) : row.attempts > 0 ? (
                      <Badge variant="destructive">not posted</Badge>
                    ) : (
                      <Badge variant="secondary">waiting</Badge>
                    )}
                  </TableCell>
                </>
              )
            }}
          />
        )}
      </Panel>
    </div>
  )
}

/** Seconds until the next scheduled look, said the way a person would say it. */
function countdown(at: number): string {
  const secs = Math.max(0, at - Math.floor(Date.now() / 1000))
  if (secs < 90) return 'a few seconds'
  const mins = Math.round(secs / 60)
  if (mins < 90) return `${mins} min`
  return `${Math.round(mins / 60)} h`
}
