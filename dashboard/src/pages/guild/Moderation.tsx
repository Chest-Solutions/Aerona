/**
 * The warn ledger and the escalation ladder.
 *
 * A warn written here is a row in this install's database, not a message from the bot — the
 * bot escalates on its own once a member passes the threshold, which is why the form below
 * configures a *ladder* and the list above is only the record.
 */
import { useState } from 'react'
import { toast } from 'sonner'
import { ListChecks, Trash2 } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { TableCell } from '@/components/ui/table'
import { Textarea } from '@/components/ui/textarea'

import { ApiError, clearWarns, createWarn, pardonWarn, warns, type WarnRow } from '../../lib/api'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { SettingsForm } from '../../lib/settings-form'
import { ago, PageHeader, Panel, shortId, Table } from './shared'

const SNOWFLAKE = /^[0-9]{1,20}$/

export default function Moderation() {
  const { id } = useGuild()
  const [who, setWho] = useState('')
  const [looked, setLooked] = useState('')
  const [reason, setReason] = useState('')
  const [busy, setBusy] = useState(false)
  const list = useAsync((signal) => warns(id, looked || undefined, 100), [id, looked])
  const rows = (list.data?.warns ?? []) as WarnRow[]
  const counts = list.data?.counts ?? null
  const invalid = looked !== '' && !SNOWFLAKE.test(looked)

  const submit = async () => {
    if (!SNOWFLAKE.test(who.trim())) {
      toast.error('Paste the member’s id — 18 digits.')
      return
    }
    setBusy(true)
    try {
      const res = await createWarn(id, { user: who.trim(), reason: reason.trim() || 'no reason given' })
      toast.success(
        res.escalated
          ? `Warned · escalated: ${res.escalated}`
          : `Warned · ${res.activeCount} active, ${res.strikes} strikes`,
      )
      setReason('')
      list.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  const pardon = async (row: WarnRow) => {
    try {
      await pardonWarn(id, row.id)
      toast.success('Pardoned')
      list.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'Could not pardon.')
    }
  }

  const clear = async (user: string) => {
    try {
      const res = await clearWarns(id, user)
      toast.success(`${res.cleared} cleared`)
      list.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'Could not clear.')
    }
  }

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Moderation"
        hint="Warns, strikes and what happens at the top of the ladder."
        actions={
          counts ? (
            <>
              <Badge variant="secondary">{counts.active} active</Badge>
              <Badge variant="outline">{counts.strikes} strikes</Badge>
            </>
          ) : null
        }
      />

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Warn someone</CardTitle>
          <CardDescription>The bot sends the reason as a DM if that is on. Nobody else is told.</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_2fr_auto] sm:items-end">
          <Label className="grid gap-1.5">
            <span className="text-sm font-medium">Member id</span>
            <Input
              value={who}
              onChange={(e) => setWho(e.target.value)}
              onBlur={() => setLooked(who.trim())}
              placeholder="123456789012345678"
              inputMode="numeric"
            />
          </Label>
          <Label className="grid gap-1.5">
            <span className="text-sm font-medium">Reason</span>
            <Textarea rows={2} value={reason} onChange={(e) => setReason(e.target.value)} placeholder="What they did" />
          </Label>
          <div className="grid gap-2">
            <Button onClick={submit} disabled={busy || invalid}>
              Warn
            </Button>
            {looked ? (
              <Button variant="ghost" size="sm" onClick={() => void clear(looked)}>
                <Trash2 className="size-4" /> Clear this member
              </Button>
            ) : null}
          </div>
        </CardContent>
      </Card>

      <Panel
        title="Ledger"
        hint={list.loading ? 'Loading…' : `${rows.length} shown · ${ago(latest(rows))} last`}
        actions={
          <Button variant="outline" size="sm" onClick={() => list.reload()} disabled={list.loading}>
            Reload
          </Button>
        }
      >
        <Table
          head={['Member', 'Reason', 'By', 'Given', 'State', '']}
          rows={rows}
          render={(raw) => {
            const r = raw as WarnRow
            return (
              <>
                <TableCell title={r.user_id}>
                  <button className="font-medium underline-offset-2 hover:underline" onClick={() => setLooked(r.user_id)}>
                    {shortId(r.user_id)}
                  </button>
                </TableCell>
                <TableCell className="max-w-[36ch] truncate" title={r.reason}>
                  {r.reason}
                </TableCell>
                <TableCell className="text-muted-foreground" title={r.moderator_id}>
                  {r.moderator_id === '0' ? 'bot' : shortId(r.moderator_id)}
                </TableCell>
                <TableCell className="text-muted-foreground">{ago(r.created_at)}</TableCell>
                <TableCell>
                  {r.active ? <Badge>active</Badge> : <Badge variant="outline">closed</Badge>}
                </TableCell>
                <TableCell className="text-end">
                  {r.active ? (
                    <Button variant="ghost" size="sm" onClick={() => void pardon(r)}>
                      Pardon
                    </Button>
                  ) : null}
                </TableCell>
              </>
            )
          }}
        />
      </Panel>

      <SettingsForm
        feature="moderation"
        title="Staff, channels and the ladder"
        icon={ListChecks}
        tone="warn"
        description="Everything below is this server only."
        fields={[
          { kind: 'ids', key: 'log_channel_id', label: 'Mod log channel', hint: 'Sanctions and cleared warnings are echoed here.', source: 'channels' },
          { kind: 'ids', key: 'mod_role_id', label: 'Staff role', hint: 'Members with it can run the moderation commands.', source: 'roles' },
          { kind: 'int', key: 'warn_expiry_days', label: 'Warns expire after', hint: '0 keeps them forever.', min: 0, max: 3650, suffix: 'days' },
          { kind: 'int', key: 'strike_threshold', label: 'Escalate at', hint: 'How many active warns tip it over.', min: 1, max: 20, suffix: 'warns' },
          {
            kind: 'enum',
            key: 'escalate_action',
            label: 'Then',
            hint: 'What the bot does the moment the threshold is crossed.',
            options: [
              { value: 'none', label: 'Nothing, just log it' },
              { value: 'timeout', label: 'Timeout' },
              { value: 'kick', label: 'Kick' },
              { value: 'ban', label: 'Ban' },
            ],
          },
          { kind: 'switch', key: 'dm_reason', label: 'DM the member the reason', hint: 'Skipped when their DMs are closed.' },
          { kind: 'switch', key: 'hierarchy_check', label: 'Never act above my role', hint: 'Leave this on unless you know why it is off.' },
          { kind: 'int', key: 'purge_max', label: 'Purge up to', min: 1, max: 1000, suffix: 'messages' },
          { kind: 'int', key: 'purge_min_age_secs', label: 'Skip messages younger than', hint: 'Discord refuses to bulk-delete anything under 14 days anyway.', min: 0, max: 604800, suffix: 'secs' },
        ]}
      />
    </div>
  )
}

function latest(rows: WarnRow[]): number | null {
  return rows.length ? Math.max(...rows.map((r) => r.created_at)) : null
}
