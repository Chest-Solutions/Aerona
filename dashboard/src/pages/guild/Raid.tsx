/**
 * Anti-raid: notice what a join flood looks like, and one button that shuts the door.
 *
 * The lockdown button is not a preset — it is the same outbox path the detector uses, so a
 * manual freeze survives a restart and shows up in the audit trail like any other action.
 */
import { useState } from 'react'
import { toast } from 'sonner'
import { Lock, Radar, UserRoundSearch } from 'lucide-react'

import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { TableCell } from '@/components/ui/table'

import { ApiError, raidEvents, raidLockdown, type RaidEvent } from '../../lib/api'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { SettingsForm } from '../../lib/settings-form'
import { ago, PageHeader, Panel, Table } from './shared'

export default function Raid() {
  const { id, data, reload } = useGuild()
  const live = useAsync((signal) => raidEvents(id), [id])
  const events = (live.data?.events ?? []) as RaidEvent[]
  // Either "the detector is still in a raid" or "the operator froze it" — both mean the
  // same thing to a person reading this page, so they are one flag here.
  const running = (live.data?.active ?? null) as RaidEvent | null
  const active = running !== null || Boolean(data?.raid?.active)
  const [minutes, setMinutes] = useState('15')
  const [busy, setBusy] = useState(false)

  const lockdown = async (on: boolean) => {
    setBusy(true)
    try {
      const res = await raidLockdown(id, Number(minutes) || 15, on ? 'manual lockdown from the dashboard' : 'cleared from the dashboard', on)
      toast.success(on ? `Locked down · queued ${res.count} channel change(s)` : 'Slowmode cleared')
      live.reload()
      reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Anti-raid"
        hint="Join bursts, message floods, and a freeze you can apply in one click."
        actions={active ? <Badge variant="destructive">raid mode on</Badge> : <Badge variant="outline">calm</Badge>}
      />

      {active ? (
        <Alert variant="destructive">
          <Lock />
          <AlertTitle>The server is locked down</AlertTitle>
          <AlertDescription>Slowmode is up in every text channel. Clearing it puts your own settings back.</AlertDescription>
        </Alert>
      ) : null}

      <Panel title="Manual lockdown" hint="Queued through the outbox: it survives a restart, and Discord’s rate limits.">
        <div className="flex flex-wrap items-end gap-3">
          <Label className="grid gap-1.5">
            <span className="text-sm font-medium">Minutes</span>
            <Input className="w-24" value={minutes} onChange={(e) => setMinutes(e.target.value)} inputMode="numeric" />
          </Label>
          <Button onClick={() => void lockdown(true)} disabled={busy}>
            <Lock className="size-4" /> Freeze the server
          </Button>
          <Button variant="outline" onClick={() => void lockdown(false)} disabled={busy}>
            Clear it
          </Button>
        </div>
      </Panel>

      <Panel title="Last raids" hint={live.loading ? 'Loading…' : `${events.length} recorded`}>
        <Table
          head={['Started', 'Trigger', 'Peak', 'Flagged', 'Kicked', 'Banned', 'Ended']}
          rows={running ? [running, ...events.filter((e) => e.id !== running.id)] : events}
          render={(raw) => {
            const e = raw as RaidEvent
            return (
              <>
                <TableCell className="font-medium">{ago(e.started_at)}</TableCell>
                <TableCell className="text-muted-foreground">{e.trigger}</TableCell>
                <TableCell className="tabular-nums">{e.peak}</TableCell>
                <TableCell className="tabular-nums">{e.flagged}</TableCell>
                <TableCell className="tabular-nums">{e.kicked}</TableCell>
                <TableCell className="tabular-nums">{e.banned}</TableCell>
                <TableCell className="text-muted-foreground">{e.ended_at ? `${ago(e.ended_at)}` : 'running'}</TableCell>
              </>
            )
          }}
        />
      </Panel>

      <SettingsForm
        feature="anti_raid"
        title="Detection"
        icon={Radar}
        fields={[
          { kind: 'switch', key: 'enabled', label: 'Watch for raids' },
          { kind: 'switch', key: 'detect_join_burst', label: 'Join burst' },
          { kind: 'int', key: 'join_threshold', label: 'Joins that count as a burst', min: 2, max: 200, suffix: 'joins' },
          { kind: 'int', key: 'join_window_secs', label: 'Inside', min: 5, max: 600, suffix: 'secs' },
          { kind: 'switch', key: 'detect_msg_flood', label: 'Message flood', hint: 'Counted per channel, across everyone.' },
          { kind: 'int', key: 'msg_threshold', label: 'Messages that count as a flood', min: 5, max: 500 },
          { kind: 'int', key: 'msg_window_secs', label: 'Inside', min: 5, max: 300, suffix: 'secs' },
        ]}
      />

      <SettingsForm
        feature="anti_raid"
        title="Who gets caught"
        icon={UserRoundSearch}
        description="Loose enough for a normal launch day, tight enough that a raid does not fit."
        fields={[
          { kind: 'int', key: 'account_max_age_days', label: 'Flag accounts younger than', hint: '0 switches it off. Discord gives every account a creation date.', min: 0, max: 365, suffix: 'days' },
          { kind: 'int', key: 'per_user_msgs_per_sec', label: 'Or a member sending faster than', hint: 'Per second, rounded up. 0 ignores rate.', min: 0, max: 20 },
          { kind: 'switch', key: 'require_avatar', label: 'Flag joiners with no avatar' },
          { kind: 'switch', key: 'require_username_letters', label: 'Flag usernames with no letters' },
          { kind: 'lines', key: 'whitelist_user_ids', label: 'Never flag these members', hint: 'One id per line.' },
          { kind: 'lines', key: 'whitelist_role_ids', label: 'Never flag members with these roles', hint: 'One role id per line.' },
        ]}
      />

      <SettingsForm
        feature="anti_raid"
        title="What the bot does"
        icon={Lock}
        tone="warn"
        fields={[
          { kind: 'switch', key: 'auto_kick', label: 'Kick the flagged joiners' },
          { kind: 'switch', key: 'auto_ban', label: 'Ban instead of kicking', hint: 'Only do this with a tight account-age rule.' },
          { kind: 'int', key: 'mass_action_limit', label: 'Never act on more than', hint: 'A safety catch: above it the bot logs and waits for you.', min: 1, max: 200, suffix: 'members' },
          { kind: 'ids', key: 'alert_channel_id', label: 'Alert channel', source: 'channels' },
          { kind: 'int', key: 'lockdown_slowmode_secs', label: 'Slowmode during a lockdown', min: 0, max: 21600, suffix: 'secs' },
          { kind: 'switch', key: 'raise_verification', label: 'Raise the verification level too' },
          { kind: 'int', key: 'auto_recover_secs', label: 'Undo it after', hint: '0 leaves it on until you clear it.', min: 0, max: 86400, suffix: 'secs' },
          { kind: 'int', key: 'cooldown_secs', label: 'Wait between detections', min: 0, max: 3600, suffix: 'secs' },
        ]}
      />
    </div>
  )
}
