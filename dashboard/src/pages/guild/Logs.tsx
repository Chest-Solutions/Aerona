/**
 * The audit channel: what the bot repeats in a channel so staff do not have to ask.
 *
 * Twelve switches and one channel. The list is deliberately small — these are the events the
 * worker actually handles, and a toggle for something it never looks at would be a lie.
 */
import { ScrollText } from 'lucide-react'
import { toast } from 'sonner'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Label } from '@/components/ui/label'
import { Switch } from '@/components/ui/switch'
import { TableCell } from '@/components/ui/table'

import { aiEvents, patchSettings } from '../../lib/api'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { SettingsForm } from '../../lib/settings-form'
import { ago, PageHeader, Panel, shortId, Table } from './shared'

/** The same list the worker matches against, restated with the words an operator reads. */
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
  ['username_change', 'Username changes'],
  ['channel_change', 'Channels change'],
]

export default function Logs() {
  const { id, settings, reload } = useGuild()
  const cfg = settings('logging') as Record<string, unknown>
  const on = Array.isArray(cfg.events) ? (cfg.events as string[]) : []
  const recent = useAsync((signal) => aiEvents(id, 40), [id])
  const rows = (recent.data?.events ?? []) as Record<string, unknown>[]

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
        hint="Copies of what happened, in a channel your staff can read."
        actions={cfg.enabled ? <Badge variant="secondary">on</Badge> : <Badge variant="outline">off</Badge>}
      />

      <Panel title="Which events" hint="Everything you switch on is posted to the channel below.">
        <div className="grid gap-1 sm:grid-cols-2 lg:grid-cols-3">
          {EVENTS.map(([key, label]) => (
            <Label
              key={key}
              className="flex items-center justify-between gap-3 rounded-lg border px-3 py-2 text-sm font-medium hover:bg-muted/60"
            >
              {label}
              <Switch checked={on.includes(key)} onCheckedChange={() => void flip(key)} aria-label={label} />
            </Label>
          ))}
        </div>
      </Panel>

      <SettingsForm
        feature="logging"
        title="Where it goes"
        icon={ScrollText}
        fields={[
          { kind: 'switch', key: 'enabled', label: 'Post the log' },
          { kind: 'ids', key: 'channel_id', label: 'Channel', source: 'channels', hint: 'Keep it above the events, or the log buries itself.' },
          { kind: 'switch', key: 'ignore_bots', label: 'Ignore other bots', hint: 'Otherwise two bots can log each other forever.' },
          { kind: 'int', key: 'max_cached_messages', label: 'Remember messages for', hint: 'How far back an edit can be described with its old text.', min: 0, max: 5000, suffix: 'messages' },
        ]}
      />

      <Panel
        title="Recently enforced"
        hint={recent.loading ? 'Loading…' : `${rows.length} rows · the same list the filter page writes`}
        actions={
          <Button variant="outline" size="sm" onClick={recent.reload} disabled={recent.loading}>
            Reload
          </Button>
        }
      >
        <Table
          head={['When', 'Member', 'Class', 'Severity', 'Did', 'Where']}
          rows={rows}
          render={(r) => {
            const row = r as Record<string, unknown>
            return (
              <>
                <TableCell className="text-muted-foreground">{ago(Number(row.at ?? 0))}</TableCell>
                <TableCell className="font-medium" title={String(row.userId ?? '')}>
                  {String(row.user ?? shortId(String(row.userId ?? '')))}
                </TableCell>
                <TableCell>{String(row.category ?? '—')}</TableCell>
                <TableCell>
                  <Badge variant={row.severity === 'critical' || row.severity === 'high' ? 'destructive' : 'outline'}>
                    {String(row.severity ?? '—')}
                  </Badge>
                </TableCell>
                <TableCell className="text-muted-foreground">{String(row.action ?? '—')}</TableCell>
                <TableCell className="text-muted-foreground" title={String(row.channel_id ?? '')}>
                  #{shortId(String(row.channel_id ?? ''))}
                </TableCell>
              </>
            )
          }}
        />
      </Panel>
    </div>
  )
}
