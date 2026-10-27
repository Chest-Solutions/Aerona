/**
 * The server's own settings: who the bot obeys, where it stays out of the way, and how to
 * invite it in the first place.
 *
 * The invite link is built by the API rather than pasted from a docs page, so the permission
 * set the product needs and the one a guild is asked for cannot drift apart.
 */
import { useState } from 'react'
import { Check, Copy, EyeOff, KeyRound, Plus } from 'lucide-react'
import { toast } from 'sonner'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { TableCell } from '@/components/ui/table'

import { settingsHistory, type HistoryRow } from '../../lib/api'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { SettingsForm } from '../../lib/settings-form'
import { ago, PageHeader, Panel, Table } from './shared'

export default function Settings() {
  const { id, name, inviteUrl, guilds } = useGuild()
  const row = guilds.find((g) => g.id === id) ?? null
  const [copied, setCopied] = useState(false)
  const history = useAsync((signal) => settingsHistory(id, 'general'), [id])
  const rows = (history.data?.history ?? []) as HistoryRow[]

  const copy = async () => {
    if (!inviteUrl) return
    try {
      await navigator.clipboard.writeText(inviteUrl)
      setCopied(true)
      setTimeout(() => setCopied(false), 1800)
    } catch {
      toast.error('Your browser would not let the panel copy it. Select it and copy by hand.')
    }
  }

  return (
    <div className="grid gap-5">
      <PageHeader title="Server settings" hint="Who the bot listens to, and where it keeps quiet." />

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Add Aerona to {name}</CardTitle>
          <CardDescription>Administrator, and the applications scope so slash commands work at all.</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-3">
          {inviteUrl ? (
            <>
              <code className="block overflow-x-auto rounded-lg border bg-muted/40 px-3 py-2 text-xs">{inviteUrl}</code>
              <div className="flex flex-wrap gap-2">
                <Button size="sm" onClick={copy}>
                  {copied ? <Check className="size-4" /> : <Copy className="size-4" />}
                  {copied ? 'Copied' : 'Copy link'}
                </Button>
                <Button size="sm" variant="outline" render={<a href={inviteUrl} target="_blank" rel="noreferrer" />}>
                  <Plus className="size-4" /> Open in Discord
                </Button>
              </div>
            </>
          ) : (
            <p className="text-sm text-muted-foreground">
              Aerona is already in this server, so there is nothing to invite. The link only appears for a server it is
              missing from.
            </p>
          )}
          <p className="text-xs text-muted-foreground">
            {row?.memberCount != null ? `${row.memberCount.toLocaleString()} members · ` : ''}
            {row?.botPresent ? 'Aerona is here' : 'Aerona is not here yet'}
          </p>
        </CardContent>
      </Card>

      <SettingsForm
        feature="general"
        title="Authority"
        icon={KeyRound}
        fields={[
          { kind: 'text', key: 'prefix', label: 'Text-command prefix', hint: 'Only for the non-slash commands. 1–8 characters.', max: 8, placeholder: '!' },
          { kind: 'ids', key: 'admin_role_ids', label: 'Roles that configure the bot', source: 'roles', multiple: true, hint: 'They can change every page in this panel that is not access control.' },
          { kind: 'ids', key: 'mod_role_ids', label: 'Roles that can moderate', source: 'roles', multiple: true, hint: 'Warn, mute, purge, close tickets.' },
        ]}
      />

      <SettingsForm
        feature="general"
        title="Where it stays out of the way"
        icon={EyeOff}
        fields={[
          { kind: 'ids', key: 'ignored_channel_ids', label: 'Ignore these channels', source: 'channels', multiple: true, hint: 'No filter, no stickies, no XP.' },
          { kind: 'ids', key: 'ignored_role_ids', label: 'Ignore members with these roles', source: 'roles', multiple: true },
        ]}
      />

      <Panel
        title="Change history for this server"
        hint={history.loading ? 'Loading…' : 'Every settings write, hash-chained. The full chain is on the Audit page.'}
        actions={
          <Button variant="outline" size="sm" onClick={history.reload} disabled={history.loading}>
            Reload
          </Button>
        }
      >
        <Table
          head={['When', 'Who', 'What', 'Then']}
          rows={rows}
          render={(raw, i) => {
            const r = raw as HistoryRow
            return (
              <>
                <TableCell className="text-muted-foreground">{ago(r.at)}</TableCell>
                <TableCell title={r.changedBy}>{r.changedBy === '0' ? 'bot' : r.changedBy}</TableCell>
                <TableCell>
                  <Badge variant="outline">{r.feature}</Badge>
                </TableCell>
                <TableCell className="max-w-[42ch] truncate text-xs text-muted-foreground" title={JSON.stringify(r.after)}>
                  {changed(i, rows)}
                </TableCell>
              </>
            )
          }}
        />
      </Panel>
    </div>
  )
}

/** A row of the drawer is only worth reading if it says what moved, so the newest row names
 *  the keys that differ from the one under it - and "first write" when there is nothing to
 *  compare against. */
function changed(index: number, rows: HistoryRow[]): string {
  const now = rows[index]?.after
  const prev = rows[index + 1]?.after
  if (!prev || typeof now !== 'object' || typeof prev !== 'object') return 'first write'
  const a = now as Record<string, unknown>
  const b = prev as Record<string, unknown>
  const keys = Object.keys(a).filter((k) => JSON.stringify(a[k]) !== JSON.stringify(b[k]))
  return keys.length ? keys.slice(0, 4).join(', ') + (keys.length > 4 ? ` +${keys.length - 4}` : '') : 'no fields moved'
}
