/**
 * Logging: copies of what happened, in a channel, plus the same rows the panel can search.
 *
 * There is no master switch. The channel is the switch: name one and the events below go to it,
 * clear it and nothing is recorded. A page that asked for both would allow a saved state where
 * logging is "on" and records nothing, which is the worst kind of setting.
 */
import { useState } from 'react'
import { ScrollText } from 'lucide-react'
import { toast } from 'sonner'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Label } from '@/components/ui/label'
import { Switch } from '@/components/ui/switch'
import { TableCell } from '@/components/ui/table'

import { guildLog, patchSettings, type LogRow } from '../../lib/api'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { SettingsForm } from '../../lib/settings-form'
import { ago, PageHeader, Panel, shortId, Table } from './shared'

/** The events the worker emits, in the words an operator reads. This list is checked against
 *  `aerona_core::models::LOG_EVENTS` by the prerender audit, so a switch cannot exist here for
 *  something the bot never looks at. */
const EVENTS: [string, string][] = [
  ['message_edit', 'Message edited'],
  ['message_delete', 'Message deleted'],
  ['message_bulk_delete', 'Bulk delete'],
  ['member_join', 'Member joins'],
  ['member_leave', 'Member leaves'],
  ['member_ban', 'Ban'],
  ['member_unban', 'Unban'],
  ['role_change', 'Roles change'],
  ['nickname_change', 'Nickname changes'],
  ['channel_change', 'Channels change'],
]

const label = (key: string) => EVENTS.find(([k]) => k === key)?.[1] ?? key

/** One half of a change. Both halves get a column of their own, because a log whose "before" and
 *  "after" share a cell is a log where you have to work out which line is which. */
function Side({ text, muted }: { text: string | null; muted?: boolean }) {
  if (!text) return <TableCell className="text-muted-foreground/60">-</TableCell>
  return (
    <TableCell className={`max-w-[38ch] align-top ${muted ? 'text-muted-foreground' : 'text-foreground'}`}>
      <span className="block whitespace-pre-wrap break-words">{text}</span>
    </TableCell>
  )
}

export default function Logs() {
  const { id, settings, report, reload } = useGuild()
  const cfg = settings('logging') as Record<string, unknown>
  const on = Array.isArray(cfg.events) ? (cfg.events as string[]) : []
  const channelId = cfg.channel_id ? String(cfg.channel_id) : ''
  const channels = report?.picker?.channels ?? []
  const channelName = channels.find((c) => c.id === channelId)?.name
  const [filter, setFilter] = useState('')
  const recent = useAsync((signal) => guildLog(id, { event: filter || undefined, limit: 80 }, signal), [id, filter])
  const rows = (recent.data?.events ?? []) as LogRow[]

  const flip = async (key: string) => {
    const next = on.includes(key) ? on.filter((k) => k !== key) : [...on, key]
    try {
      await patchSettings(id, 'logging', { events: next })
      reload()
    } catch {
      toast.error('Could not save.')
    }
  }

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Logging"
        actions={
          channelId ? (
            <Badge variant="secondary">to #{channelName ?? shortId(channelId)}</Badge>
          ) : (
            <Badge variant="outline">no channel picked</Badge>
          )
        }
      />

      {!channelId ? (
        <p className="rounded-xl border border-dashed px-4 py-3 text-sm text-muted-foreground">
          Nothing is recorded until you pick a channel.
        </p>
      ) : null}

      <SettingsForm
        feature="logging"
        title="Where it goes"
        icon={ScrollText}
        fields={[
          {
            kind: 'ids',
            key: 'channel_id',
            label: 'Channel',
            source: 'channels',
            hint: 'The channel is the switch: clear it and nothing is recorded.',
          },
          { kind: 'switch', key: 'ignore_bots', label: 'Ignore other bots' },
          {
            kind: 'int',
            key: 'max_cached_messages',
            label: 'Remember messages for',
            hint: 'How far back an edit can quote the old text, and how many messages the pickers can offer.',
            min: 0,
            max: 5000,
            suffix: 'messages',
          },
        ]}
      />

      <Panel title="Which events">
        <div className="grid gap-1 sm:grid-cols-2 lg:grid-cols-3">
          {EVENTS.map(([key, text]) => (
            <Label
              key={key}
              className="flex items-center justify-between gap-3 rounded-lg border px-3 py-2 text-sm font-medium hover:bg-muted/60"
            >
              {text}
              <Switch checked={on.includes(key)} onCheckedChange={() => void flip(key)} aria-label={text} />
            </Label>
          ))}
        </div>
      </Panel>

      <Panel
        title="What it recorded"
        hint={recent.loading ? 'Loading…' : `${rows.length} rows${filter ? ` · ${label(filter)}` : ''}`}
        actions={
          <Button variant="outline" size="sm" onClick={recent.reload} disabled={recent.loading}>
            Reload
          </Button>
        }
      >
        <div className="flex flex-wrap gap-1 border-b px-4 py-3">
          <Button variant={filter === '' ? 'secondary' : 'ghost'} size="sm" onClick={() => setFilter('')}>
            Everything
          </Button>
          {on.map((key) => (
            <Button
              key={key}
              variant={filter === key ? 'secondary' : 'ghost'}
              size="sm"
              onClick={() => setFilter(key)}
            >
              {label(key)}
            </Button>
          ))}
        </div>
        <Table
          head={['When', 'Member', 'What', 'Where', 'Before', 'After']}
          rows={rows}
          render={(r) => {
            const row = r as LogRow
            const where = channels.find((c) => c.id === (row.channelId ?? ''))?.name
            return (
              <>
                <TableCell className="text-muted-foreground">{ago(row.at)}</TableCell>
                <TableCell className="font-medium" title={row.userId ?? ''}>
                  {row.userId ? shortId(row.userId) : 'system'}
                </TableCell>
                <TableCell>{row.summary}</TableCell>
                <TableCell className="text-muted-foreground">
                  {where ? `#${where}` : row.channelId ? shortId(row.channelId) : ''}
                  {row.messageId ? (
                    <a
                      className="ms-1 text-xs underline decoration-muted-foreground/50"
                      href={`https://discord.com/channels/${id}/${row.channelId ?? ''}/${row.messageId}`}
                      target="_blank"
                      rel="noreferrer"
                    >
                      jump
                    </a>
                  ) : null}
                </TableCell>
                <Side text={row.before} muted />
                <Side text={row.after} />
              </>
            )
          }}
        />
        {rows.length === 0 && !recent.loading ? (
          <p className="px-4 py-3 text-sm text-muted-foreground">
            {channelId ? 'Nothing matches that filter yet.' : 'No channel picked, so nothing was recorded.'}
          </p>
        ) : null}
      </Panel>
    </div>
  )
}
