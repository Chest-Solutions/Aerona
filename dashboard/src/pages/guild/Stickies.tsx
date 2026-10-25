/**
 * Stickies: a message the bot keeps at the bottom of a channel, re-posted after N messages
 * of chatter so it never scrolls away.
 *
 * The list is the schedule, not a preview: `since count` is what the bot has counted since
 * the last repost, which is the only reason a new one appears in a channel.
 */
import { useState } from 'react'
import { toast } from 'sonner'
import { Pin, Timer, Trash2 } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { TableCell } from '@/components/ui/table'
import { Textarea } from '@/components/ui/textarea'

import { ApiError, addSticky, deleteSticky, stickies, type StickyRow } from '../../lib/api'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { SettingsForm } from '../../lib/settings-form'
import { ago, PageHeader, Panel, shortId, Table } from './shared'

export default function Stickies() {
  const { id, report } = useGuild()
  const list = useAsync((signal) => stickies(id), [id])
  const rows = (list.data?.stickies ?? []) as StickyRow[]
  const channels = report?.picker?.channels ?? []
  const [channel, setChannel] = useState('')
  const [message, setMessage] = useState('')
  const [content, setContent] = useState('')
  const [every, setEvery] = useState('25')
  const [busy, setBusy] = useState(false)

  const add = async () => {
    if (!channel || !content.trim()) {
      toast.error('Pick a channel and write the note.')
      return
    }
    setBusy(true)
    try {
      await addSticky(id, { channel, message: message.trim() || 'A pinned note', content: content.trim(), everyMessages: Number(every) || 25 })
      toast.success('Sticky added')
      setContent('')
      setMessage('')
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

  const name = (chan: string) => channels.find((c) => c.id === chan)?.name ?? shortId(chan)

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Stickies"
        hint="Rules that stay where people can see them."
        actions={<Badge variant="secondary">{rows.length} running</Badge>}
      />

      <Panel title="Add one" hint="The bot posts it once, then keeps it fresh below the last message.">
        <div className="grid gap-3 sm:grid-cols-2">
          <div className="grid gap-1.5">
            <span className="text-sm font-medium">Channel</span>
            <Select value={channel} onValueChange={(v) => setChannel(String(v ?? ''))}>
              <SelectTrigger className="w-full">
                <SelectValue placeholder="Pick a channel…" />
              </SelectTrigger>
              <SelectContent>
                {channels.map((c) => (
                  <SelectItem key={c.id} value={c.id}>
                    #{c.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            {channels.length === 0 ? (
              <span className="text-xs text-muted-foreground">The bot has not reported this server’s channels yet.</span>
            ) : null}
          </div>
          <Label className="grid gap-1.5">
            <span className="text-sm font-medium">Repost after</span>
            <Input value={every} onChange={(e) => setEvery(e.target.value)} inputMode="numeric" placeholder="25" />
            <span className="text-xs text-muted-foreground">messages. Discord caps a channel at 50 pins.</span>
          </Label>
        </div>
        <Label className="grid gap-1.5">
          <span className="text-sm font-medium">The note</span>
          <Textarea rows={3} value={content} onChange={(e) => setContent(e.target.value)} placeholder="Read #rules before posting. No NSFW. Be decent." />
        </Label>
        <div className="flex items-center gap-2">
          <Button onClick={add} disabled={busy}>
            <Pin className="size-4" /> Add sticky
          </Button>
          <Input
            className="max-w-[22ch]"
            value={message}
            onChange={(e) => setMessage(e.target.value)}
            placeholder="label in the list"
          />
        </div>
      </Panel>

      <Panel title="Running" hint={list.loading ? 'Loading…' : `${rows.length} in this server`}>
        <Table
          head={['Channel', 'Note', 'Every', 'Since', 'Last repost', '']}
          rows={rows}
          render={(raw) => {
            const s = raw as StickyRow
            return (
              <>
                <TableCell className="font-medium">#{name(s.channel_id)}</TableCell>
                <TableCell className="max-w-[40ch] truncate" title={s.content}>
                  {s.content}
                </TableCell>
                <TableCell className="tabular-nums text-muted-foreground">{s.every_messages}</TableCell>
                <TableCell className="tabular-nums">{s.since_count}</TableCell>
                <TableCell className="text-muted-foreground">{ago(s.last_run_at)}</TableCell>
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
          { kind: 'int', key: 'cooldown_secs', label: 'Never repost sooner than', hint: 'Keeps two bursts of chatter from double-posting.', min: 0, max: 3600, suffix: 'secs' },
          { kind: 'int', key: 'max_per_channel', label: 'Most stickies per channel', min: 1, max: 10, suffix: 'notes' },
          { kind: 'switch', key: 'repost', label: 'Delete the old copy when reposting', hint: 'Off leaves it in the scrollback.' },
        ]}
      />
    </div>
  )
}
