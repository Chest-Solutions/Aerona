/**
 * Tickets: the private channel a member gets when they need help, and the record of it.
 *
 * The board below is the same table the bot's `/ticket` command writes to, so a claim made here is
 * a claim the bot honours in Discord - one queue, two doors. A *type* is one button on that board:
 * it carries its own colour, staff role and opening text, which is how "report a player" and
 * "billing" are different queues without being different products.
 */
import { useMemo, useState } from 'react'
import { toast } from 'sonner'
import { Plus, Ticket as TicketGlyph, Trash2 } from 'lucide-react'

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

import {
  addTicketPanel,
  ApiError,
  claimTicket,
  deleteTicketPanel,
  patchSettings,
  setTicketStatus,
  ticketPanels,
  ticketTranscriptUrl,
  tickets,
  unclaimTicket,
  type Ticket,
  type TicketPanelRow,
} from '../../lib/api'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { FieldHint, SettingsForm } from '../../lib/settings-form'
import { ago, ColorField, DiscordEmbedPreview, inTime, PageHeader, Panel, shortId, Table } from './shared'

/** One type's button, drawn the way Discord will draw it. The card is the real component rather than
 *  an approximation of one, because "the colour does not show" was a preview problem as much as a
 *  Discord one: a hand-drawn bar fills the whole left edge, Discord's fills four pixels. */
function BoardPreview({
  rows,
  color,
  label,
}: {
  rows: TicketPanelRow[]
  color: number
  label: string
}) {
  const swatch = (c: number) => `rgb(${(c >> 16) & 255}, ${(c >> 8) & 255}, ${c & 255})`
  return (
    <div className="grid max-w-[64ch] gap-2">
      <DiscordEmbedPreview color={color} body="Pick what you need help with and a channel will be made for it." author="Aerona" />
      <div className="flex flex-wrap gap-1.5">
        {rows.length ? (
          rows.map((r) => (
            <span
              key={r.id}
              className="rounded-md border px-2.5 py-1 text-xs"
              style={r.color ? { background: swatch(r.color), borderColor: swatch(r.color), color: '#fff' } : undefined}
            >
              {r.emoji ? `${r.emoji} ` : ''}
              {r.label}
            </span>
          ))
        ) : (
          <span className="rounded-md bg-primary px-2.5 py-1 text-xs text-primary-foreground">{label || 'Open a ticket'}</span>
        )}
      </div>
    </div>
  )
}

export default function Tickets() {
  const { id, settings, report } = useGuild()
  const cfg = settings('tickets') as Record<string, unknown>
  const list = useAsync((signal) => tickets(id, { status: '', limit: 50 }), [id])
  const rows = (list.data?.tickets ?? []) as Ticket[]
  const counts = list.data?.counts ?? null
  const panels = useAsync((signal) => ticketPanels(id), [id])
  const types = panels.data?.panels ?? []
  const channels = report?.picker?.channels ?? []
  const categories = report?.picker?.categories ?? []
  const roles = (report?.picker?.roles ?? []).filter((r) => r.assignable !== false)

  const [label, setLabel] = useState('')
  const [emoji, setEmoji] = useState('')
  const [channel, setChannel] = useState('')
  const [role, setRole] = useState('')
  const [subject, setSubject] = useState('')
  const [intro, setIntro] = useState('')
  const [color, setColor] = useState(0x58_65_f2)
  const [busy, setBusy] = useState(false)

  const grouped = useMemo(() => {
    const names = new Map(categories.map((c) => [c.id, c.name]))
    const groups = new Map<string, typeof channels>()
    for (const c of channels) {
      const key = (c.category && names.get(c.category)) || 'Unfiled'
      const g = groups.get(key) ?? []
      g.push(c)
      groups.set(key, g)
    }
    return [...groups.entries()]
  }, [channels, categories])

  /** `label | https://url` per line, which is how every other list on this panel is edited. The
   *  server decides what is usable; a line with no `|` is kept with the url as its label. */
  const linkText = useMemo(
    () =>
      ((cfg.links as { label: string; url: string }[] | undefined) ?? [])
        .map((l) => `${l.label} | ${l.url}`)
        .join('\n'),
    [cfg.links],
  )

  /** The board's home, from the settings document: the composer and the guard below both read it,
   *  and the channel id arrives as a string because that is what the model stores. */
  const board = typeof cfg.board_channel_id === 'string' ? cfg.board_channel_id : ''

  const addType = async () => {
    // The channel is optional the moment the page has a ticket channel to put the board in: one
    // place to decide where the buttons live, and a type that wants its own channel still says so.
    if (!label.trim()) {
      toast.error('A type needs a name.')
      return
    }
    if (!channel && !board) {
      toast.error('Pick a channel for this type, or set the ticket channel in the settings below.')
      return
    }
    setBusy(true)
    try {
      await addTicketPanel(id, {
        channel: channel || undefined,
        label: label.trim(),
        emoji: emoji.trim() || undefined,
        supportRole: role || undefined,
        color,
        subject: subject.trim() || undefined,
        intro: intro.trim() || undefined,
      })
      toast.success('Type added - the bot posts the button')
      setLabel('')
      setChannel('')
      setEmoji('')
      setSubject('')
      setIntro('')
      panels.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  const dropType = async (row: TicketPanelRow) => {
    try {
      await deleteTicketPanel(id, row.id)
      toast.success('Removed')
      panels.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'Could not remove it.')
    }
  }

  const saveLinks = async (raw: string) => {
    const links = raw
      .split('\n')
      .map((line) => line.trim())
      .filter(Boolean)
      .map((line) => {
        const parts = line.split('|').map((p) => p.trim())
        // One column means the whole line is the URL and the label is its own first characters;
        // two columns is the intended shape, `Rules | https://...`.
        const head = parts[0] ?? ''
        const url = parts[1] || head
        const text = parts[1] ? head : head.slice(0, 40)
        return { label: text, url }
      })
    try {
      await patchSettings(id, 'tickets', { links: links })
      toast.success(links.length ? `${links.length} link button(s) saved` : 'Links cleared')
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'Could not save.')
    }
  }

  const act = async (verb: string, fn: () => Promise<unknown>) => {
    try {
      await fn()
      toast.success(verb)
      list.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'The API did not answer.')
    }
  }

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Tickets"
        actions={
          counts ? (
            <>
              <Badge variant="secondary">{counts.open} open</Badge>
              <Badge variant="outline">{counts.pending} pending</Badge>
            </>
          ) : null
        }
      />

      {!cfg.enabled ? (
        <p className="rounded-xl border border-dashed px-4 py-3 text-sm text-muted-foreground">
          Tickets are switched off.
        </p>
      ) : null}

      <Panel title="Board" hint={list.loading ? 'Loading…' : `${rows.length} recent · newest ${ago(rows[0]?.createdAt)}`}>
        <Table
          head={['Ref', 'Subject', 'Opened by', 'Claimed', 'Status', 'Msgs', '']}
          rows={rows}
          render={(raw) => {
            const t = raw as Ticket
            return (
              <>
                <TableCell className="font-medium">
                  {t.reference}
                  {/* What happens to this channel next, on the row that lists it. A ticket whose
                      channel is gone is not a ticket whose link is broken, and saying which of the
                      two it is costs one line here and saves a support message. */}
                  {t.channelDeletedAt ? (
                    <div className="text-[11px] font-normal text-muted-foreground">
                      channel deleted {ago(t.channelDeletedAt)} · transcript kept
                    </div>
                  ) : t.deleteAt ? (
                    <div className="text-[11px] font-normal text-muted-foreground">
                      channel deletes {inTime(t.deleteAt)}
                    </div>
                  ) : null}
                </TableCell>
                <TableCell className="max-w-[30ch] truncate" title={t.subject}>
                  {t.subject}
                </TableCell>
                <TableCell className="text-muted-foreground" title={t.ownerId}>
                  {shortId(t.ownerId)}
                </TableCell>
                <TableCell className="text-muted-foreground" title={t.claimedBy ?? ''}>
                  {t.claimedBy ? shortId(t.claimedBy) : '-'}
                </TableCell>
                <TableCell>
                  <Badge variant={t.status === 'open' ? 'default' : t.status === 'pending' ? 'secondary' : 'outline'}>
                    {t.status}
                  </Badge>
                </TableCell>
                <TableCell className="tabular-nums">{t.messageCount}</TableCell>
                <TableCell className="text-end whitespace-nowrap">
                  {t.status !== 'closed' ? (
                    t.claimedBy ? (
                      <Button variant="ghost" size="sm" onClick={() => void act('Released', () => unclaimTicket(id, t.id))}>
                        Release
                      </Button>
                    ) : (
                      <Button variant="ghost" size="sm" onClick={() => void act('Claimed', () => claimTicket(id, t.id))}>
                        Claim
                      </Button>
                    )
                  ) : null}
                  {t.status !== 'closed' ? (
                    <Button variant="ghost" size="sm" onClick={() => void act('Closed', () => setTicketStatus(id, t.id, 'closed'))}>
                      Close
                    </Button>
                  ) : (
                    <>
                      <Button variant="ghost" size="sm" render={<a href={ticketTranscriptUrl(id, t.id)} />}>
                        Transcript
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        title="A file copy, for a case folder or an email"
                        render={<a href={ticketTranscriptUrl(id, t.id, undefined, true)} />}
                      >
                        Save
                      </Button>
                    </>
                  )}
                </TableCell>
              </>
            )
          }}
        />
      </Panel>

      <Panel
        title="Ticket types"
        hint={types.length ? `${types.length} button(s) on the board` : 'One button per type'}
      >
        {types.length ? (
          <Table
            head={['Button', 'Channel', 'Staff role', 'Card', '']}
            rows={types}
            render={(raw) => {
              const t = raw as TicketPanelRow
              return (
                <>
                  <TableCell className="font-medium">
                    {t.color ? (
                      <span
                        aria-hidden
                        className="me-1.5 inline-block size-2.5 rounded-full align-middle"
                        style={{ background: `rgb(${(t.color >> 16) & 255}, ${(t.color >> 8) & 255}, ${t.color & 255})` }}
                      />
                    ) : null}
                    {t.emoji ? `${t.emoji} ` : ''}
                    {t.label}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {channels.find((c) => c.id === t.channel_id)?.name
                      ? `#${channels.find((c) => c.id === t.channel_id)!.name}`
                      : shortId(t.channel_id)}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {t.support_role_id
                      ? roles.find((r) => r.id === t.support_role_id)?.name ?? shortId(t.support_role_id)
                      : 'the default'}
                  </TableCell>
                  <TableCell className="max-w-[34ch] truncate text-muted-foreground" title={t.intro || t.subject}>
                    {t.intro || t.subject || 'the default card'}
                  </TableCell>
                  <TableCell className="text-end">
                    <Button variant="ghost" size="sm" onClick={() => void dropType(t)}>
                      <Trash2 className="size-4" />
                    </Button>
                  </TableCell>
                </>
              )
            }}
          />
        ) : null}

        <div className="grid gap-3 border-t pt-3 sm:grid-cols-2">
          <label className="grid gap-1.5">
            <span className="text-sm font-medium">Button label</span>
            <Input value={label} onChange={(e) => setLabel(e.target.value)} maxLength={80} placeholder="Report a player" />
          </label>
          <label className="grid gap-1.5">
            <span className="text-sm font-medium">Emoji</span>
            <Input value={emoji} onChange={(e) => setEmoji(e.target.value)} maxLength={8} placeholder="🚩" />
          </label>
          <div className="grid gap-1.5">
            <span className="flex items-center gap-1.5 text-sm font-medium">
              Channel it lives in
              <FieldHint text="Only the board: the ticket channels themselves are made under the category, in private, for the member and the staff role." />
            </span>
            <Select value={channel} onValueChange={(v) => setChannel(String(v ?? ''))}>
              <SelectTrigger className="w-full">
                <SelectValue
                  placeholder={
                    channels.length === 0
                      ? 'No channels reported yet'
                      : board
                        ? 'The ticket channel'
                        : 'Pick a channel…'
                  }
                />
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
          <div className="grid gap-1.5">
            <span className="flex items-center gap-1.5 text-sm font-medium">
              Staff role
              <FieldHint text="Whoever holds it sees this button on the board and can claim the tickets it opens." />
            </span>
            <Select value={role} onValueChange={(v) => setRole(String(v ?? ''))}>
              <SelectTrigger className="w-full">
                <SelectValue placeholder="The default staff role" />
              </SelectTrigger>
              <SelectContent>
                {roles.map((r) => (
                  <SelectItem key={r.id} value={r.id}>
                    {r.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <label className="grid gap-1.5">
            <span className="text-sm font-medium">Subject</span>
            <Input value={subject} onChange={(e) => setSubject(e.target.value)} maxLength={200} placeholder="Report a player" />
          </label>
          <ColorField label="Card colour" value={color} onChange={setColor} />
        </div>
        <label className="grid gap-1.5">
          <span className="text-sm font-medium">Opening text</span>
          <Textarea
            rows={2}
            maxLength={1500}
            value={intro}
            onChange={(e) => setIntro(e.target.value)}
            placeholder="Say who to tag and what to include. {user} {ticket} {subject} {server} work here."
          />
        </label>
        <div className="flex items-center gap-3">
          <Button onClick={addType} disabled={busy}>
            <Plus className="size-4" /> Add type
          </Button>
          <BoardPreview rows={types} color={color} label={label} />
        </div>
      </Panel>

      <SettingsForm
        feature="tickets"
        title="The card and its buttons"
        icon={TicketGlyph}
        tone="warn"
        fields={[
          { kind: 'switch', key: 'enabled', label: 'Turn tickets on' },
          {
            kind: 'ids',
            key: 'board_channel_id',
            label: 'Ticket channel',
            source: 'channels',
            hint: 'Where the board of buttons is posted. Saving puts it there straight away, and any type without a channel of its own goes here too.',
          },
          { kind: 'ids', key: 'category_id', label: 'Category', source: 'categories' },
          { kind: 'ids', key: 'support_role_id', label: 'Staff role', source: 'roles' },
          { kind: 'ids', key: 'transcript_channel_id', label: 'Transcripts to', source: 'channels' },
          {
            kind: 'int',
            key: 'delete_after_minutes',
            label: 'Delete the channel after',
            hint: 'Counted from the moment staff close the ticket, and cancelled if the member writes again first. The transcript and its pictures stay in Aerona either way; 0 keeps the channel.',
            min: 0,
            max: 10080,
            suffix: 'minutes',
          },
          {
            kind: 'switch',
            key: 'keep_ticket_files',
            label: 'Keep the pictures the ticket contained',
            hint: 'A Discord attachment is readable while its message is. A deleted channel takes its files with it, so the transcript copies the images into itself and shows them offline, forever.',
          },
          {
            kind: 'int',
            key: 'keep_ticket_files_days',
            label: 'Keep those copies for',
            hint: '0 keeps them as long as the ticket record. Shorter than that and an old transcript keeps its words and loses its pictures to a link that may already be dead - which it says in its footer rather than pretending.',
            min: 0,
            max: 36500,
            suffix: 'days',
          },
          {
            kind: 'text',
            key: 'name_format',
            label: 'Channel name',
            hint: 'Tokens: {ref} {user} {subject}.',
            placeholder: 'ticket-{ref}-{user}',
          },
          { kind: 'int', key: 'max_per_user', label: 'Open per member', min: 1, max: 10, suffix: 'tickets' },
          {
            kind: 'int',
            key: 'auto_close_days',
            label: 'Auto-close after',
            hint: '0 leaves them open until staff act.',
            min: 0,
            max: 90,
            suffix: 'days idle',
          },
          {
            kind: 'switch',
            key: 'claim_required',
            label: 'Answering requires a claim',
            hint: 'Stops two staff replying at once.',
          },
          { kind: 'switch', key: 'show_claim', label: 'Claim button' },
          { kind: 'switch', key: 'show_close', label: 'Close button' },
          { kind: 'switch', key: 'show_transcript', label: 'Transcript button' },
          { kind: 'text', key: 'claim_label', label: 'Claim says', max: 80, placeholder: 'Claim' },
          { kind: 'text', key: 'close_label', label: 'Close says', max: 80, placeholder: 'Close ticket' },
          {
            kind: 'text',
            key: 'transcript_label',
            label: 'Transcript says',
            max: 80,
            placeholder: 'Transcript',
          },
          { kind: 'switch', key: 'ping_support_role', label: 'Ping the staff role on open' },
          {
            kind: 'switch',
            key: 'use_two_channels',
            label: 'Private channel plus a public one',
            hint: 'The member writes in the public one; staff answer in the private one.',
          },
          {
            kind: 'color',
            key: 'embed_color',
            label: 'Card colour',
            hint: 'Discord tints the four pixel bar on the left of the card and nothing else, so a pale colour reads as no colour in a light theme. The preview above is drawn the way Discord draws it.',
          },
          {
            kind: 'text',
            key: 'open_title',
            label: 'Opening card title',
            hint: 'Tokens: {ticket} {user} {subject} {server}.',
            placeholder: 'Ticket {ticket} for {user}',
            max: 180,
          },
          {
            kind: 'textarea',
            key: 'default_message',
            label: 'Opening card text',
            rows: 2,
            max: 500,
          },
          { kind: 'text', key: 'close_title', label: 'Closing card title', max: 180 },
          { kind: 'textarea', key: 'close_body', label: 'Closing card text', rows: 2, max: 500 },
          {
            kind: 'enum',
            key: 'transcript_format',
            label: 'Transcript file',
            hint: 'HTML opens in a browser and shows the pictures that were in the channel.',
            options: [
              { value: 'html', label: 'HTML, with images' },
              { value: 'text', label: 'Plain text' },
              { value: 'json', label: 'JSON' },
            ],
          },
        ]}
      />

      <Panel
        title="Link buttons"
      >
        <label className="grid gap-1.5">
          <span className="sr-only">Link buttons, one per line</span>
          <Textarea
            rows={3}
            defaultValue={linkText}
            key={linkText}
            placeholder={'Rules | https://discord.com/channels/1/2\nStatus | https://status.example.com'}
            onBlur={(e) => void saveLinks(e.target.value)}
          />
        </label>
      </Panel>
    </div>
  )
}
