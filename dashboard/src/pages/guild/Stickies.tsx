/**
 * Stickies: a note the bot keeps at the bottom of a channel.
 *
 * You write it here. The bot posts it, and when the channel has moved on it sends a fresh copy and
 * deletes the one it made before, so the newest text is always the last thing in the channel.
 * There is no switch for that: the alternative was leaving a stale copy at the top of the channel,
 * which is the one thing a sticky exists to prevent. Nothing here needs a message id, a pin, or a
 * trip into Discord.
 */
import { useMemo, useState } from 'react'
import { toast } from 'sonner'
import { Pin, Timer, Trash2 } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectGroupLabel,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { TableCell } from '@/components/ui/table'
import { Textarea } from '@/components/ui/textarea'

import { ApiError, addSticky, deleteSticky, stickies, type StickyRow } from '../../lib/api'
import { refreshPicker } from '../../lib/picker-sync'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { FieldHint, SettingsForm } from '../../lib/settings-form'
import { ago, ColorField, DiscordEmbedPreview, PageHeader, Panel, shortId, Table } from './shared'

export default function Stickies() {
  const { id, report, reload } = useGuild()
  const list = useAsync((signal) => stickies(id), [id])
  const rows = list.data?.stickies ?? []
  const channels = report?.picker?.channels ?? []
  const categories = report?.picker?.categories ?? []
  const [channel, setChannel] = useState('')
  const [content, setContent] = useState('')
  const [title, setTitle] = useState('')
  const [body, setBody] = useState('')
  // `null` is not a colour: it is "no card, plain text". Blurple is what Discord's own composer
  // starts a new embed on, so that is what the swatch shows - but it is not *sent* until somebody
  // touches it, or every sticky this page ever made would arrive as a card.
  const [color, setColor] = useState<number | null>(null)
  const [every, setEvery] = useState('25')
  const [busy, setBusy] = useState(false)
  const [syncing, setSyncing] = useState(false)

  /** Channels under their category heading, in Discord's order, because a flat list of 40
   *  `#general`s is not a list anybody can pick from. */
  const grouped = useMemo(() => {
    const names = new Map(categories.map((c) => [c.id, c.name]))
    const groups = new Map<string, typeof channels>()
    for (const c of channels) {
      const key = (c.category && names.get(c.category)) || 'Unfiled'
      const list = groups.get(key) ?? []
      list.push(c)
      groups.set(key, list)
    }
    return [...groups.entries()]
  }, [channels, categories])

  /** The bot's cache is what knows this server's channels. Refresh asks it to look again and
   *  waits for the report to move, so the list either fills up or says the bot is not answering. */
  const sync = async () => {
    setSyncing(true)
    if (!(await refreshPicker(id, report?.picker?.reportedAt, reload))) {
      toast.error('The bot did not answer.')
    }
    setSyncing(false)
  }

  const add = async () => {
    if (!channel) {
      toast.error('Pick a channel first.')
      return
    }
    if (!content.trim() && !title.trim() && !body.trim()) {
      toast.error('Write something to post, or an embed, or both.')
      return
    }
    setBusy(true)
    try {
      await addSticky(id, {
        channel,
        content: content.trim() || undefined,
        embed_title: title.trim() || undefined,
        embed_body: body.trim() || undefined,
        // A colour of its own is an embed: Discord's card with a bar and no text is a thing people
        // want (it marks the note without repeating it), and gating the colour on the text being
        // filled in is how "I set white and nothing changed" happened.
        embed_color: color ?? undefined,
        everyMessages: Number(every) || 25,
      })
      toast.success('Sticky added')
      setContent('')
      setTitle('')
      setBody('')
      setColor(null)
      list.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  const drop = async (row: StickyRow) => {
    try {
      await deleteSticky(id, row.id)
      toast.success('Removed')
      list.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'Could not remove it.')
    }
  }

  /** The name the worker reported, or the id it was filed under. Never a blank. */
  const name = (row: StickyRow) =>
    row.channelName
      ? `#${row.channelName}`
      : channels.find((c) => c.id === row.channel_id)?.name
        ? `#${channels.find((c) => c.id === row.channel_id)!.name}`
        : shortId(row.channel_id)

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Stickies"
        actions={<Badge variant="secondary">{rows.length} running</Badge>}
      />

      <Panel
        title="Add one"
        actions={
          <Button type="button" variant="ghost" size="sm" onClick={() => void sync()} disabled={syncing}>
            {syncing ? 'Asking the bot' : 'Refresh channels'}
          </Button>
        }
      >
        <div className="grid gap-3 sm:grid-cols-2">
          <div className="grid gap-1.5">
            <span className="flex items-center gap-1.5 text-sm font-medium">
              Channel
              <FieldHint text="The bot posts its note at the bottom of this channel and keeps it there." />
            </span>
            <Select value={channel} onValueChange={(v) => setChannel(String(v ?? ''))}>
              <SelectTrigger className="w-full">
                <SelectValue placeholder={channels.length ? 'Pick a channel…' : 'No channels reported yet'} />
              </SelectTrigger>
              <SelectContent>
                {grouped.map(([heading, list]) => (
                  <SelectGroup key={heading}>
                    <SelectGroupLabel>{heading}</SelectGroupLabel>
                    {list.map((c) => (
                      <SelectItem key={c.id} value={c.id}>
                        #{c.name}
                      </SelectItem>
                    ))}
                  </SelectGroup>
                ))}
              </SelectContent>
            </Select>
          </div>
          <label className="grid gap-1.5">
            <span className="flex items-center gap-1.5 text-sm font-medium">
              Repost after
              <FieldHint text="How many messages of chatter have to go by before the note is moved back to the bottom." />
            </span>
            <Input value={every} onChange={(e) => setEvery(e.target.value)} inputMode="numeric" placeholder="25" />
          </label>
        </div>

        <label className="grid gap-1.5">
          <span className="text-sm font-medium">Text</span>
          <Textarea
            rows={2}
            maxLength={1800}
            value={content}
            onChange={(e) => setContent(e.target.value)}
            placeholder="Rules reminders, a warning about a scam DM, whatever should not scroll away."
          />
        </label>

        <div className="grid gap-3 border-t pt-3 sm:grid-cols-2">
          <label className="grid gap-1.5">
            <span className="text-sm font-medium">Embed title</span>
            <Input value={title} onChange={(e) => setTitle(e.target.value)} maxLength={240} placeholder="Optional" />
          </label>
          <div className="flex items-end gap-1.5 pb-1">
            <ColorField label="Embed colour" value={color ?? 0x58_65_f2} onChange={(v) => setColor(v)} />
            <span className="pb-2">
              <FieldHint text="Discord tints only the four pixel bar down the left of a card, not the card. A near white colour is a near invisible bar in a light theme, and no colour at all means no bar and a plain message." />
            </span>
            {color === null ? null : (
              <Button type="button" variant="ghost" size="sm" className="mb-1" onClick={() => setColor(null)}>
                Plain text
              </Button>
            )}
          </div>
        </div>
        <label className="grid gap-1.5">
          <span className="text-sm font-medium">Embed text</span>
          <Textarea
            rows={4}
            maxLength={1800}
            value={body}
            onChange={(e) => setBody(e.target.value)}
            placeholder="Line breaks are kept inside an embed, so this can be a card and not a sentence."
          />
        </label>

        <div className="grid gap-3 sm:grid-cols-[auto_1fr] sm:items-end">
          <Button onClick={add} disabled={busy}>
            <Pin className="size-4" /> Add sticky
          </Button>
          {/* Not a drawing of what an embed is like: the component Discord's own client styles come
              from, so the bar, the width and the dark card are the ones the channel will show. */}
          <DiscordEmbedPreview color={color} title={title} body={body || content} author="Aerona" />
        </div>
      </Panel>

      <Panel title="Running" hint={list.loading ? 'Loading…' : `${rows.length} in this server`}>
        <Table
          head={['Channel', 'Says', 'Every', 'Since', 'Last posted', '']}
          rows={rows}
          render={(raw) => {
            const s = raw as StickyRow
            const said = s.content.trim() || s.embed_title?.trim() || s.embed_body?.trim() || ''
            // The same rule the worker applies: a colour is a card on its own.
            const card = Boolean(s.embed_title?.trim() || s.embed_body?.trim() || (s.embed_color ?? null) !== null)
            return (
              <>
                <TableCell className="font-medium">{name(s)}</TableCell>
                <TableCell className="max-w-[44ch] truncate" title={said}>
                  {said}
                  {card ? <Badge variant="outline">embed</Badge> : null}
                </TableCell>
                <TableCell className="tabular-nums text-muted-foreground">{s.every_messages}</TableCell>
                <TableCell className="tabular-nums">{s.since_count}</TableCell>
                <TableCell className="text-muted-foreground">{s.last_run_at ? ago(s.last_run_at) : 'not posted yet'}</TableCell>
                <TableCell className="text-end">
                  <Button variant="ghost" size="sm" onClick={() => void drop(s)}>
                    <Trash2 className="size-4" />
                  </Button>
                </TableCell>
              </>
            )
          }}
        />
      </Panel>

      <SettingsForm
        feature="sticky"
        title="Behaviour"
        icon={Timer}
        fields={[
          { kind: 'switch', key: 'enabled', label: 'Run stickies in this server' },
          {
            kind: 'int',
            key: 'cooldown_secs',
            label: 'Never repost sooner than',
            hint: 'Two bursts of chatter in one channel count as one repost, so a sticky cannot become a metronome.',
            min: 0,
            max: 3600,
            suffix: 'secs',
          },
          { kind: 'int', key: 'max_per_channel', label: 'Most stickies per channel', min: 1, max: 10, suffix: 'notes' },
        ]}
      />
    </div>
  )
}
