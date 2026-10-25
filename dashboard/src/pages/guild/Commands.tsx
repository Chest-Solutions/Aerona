/**
 * The slash commands this server has.
 *
 * The list is not read from this build: it is what the worker last got back from Discord after
 * `set_commands`, with the time it asked. So the page can say "12 commands, 4 min ago" and be
 * right about a server where somebody disabled four of them an hour ago — and when the worker
 * has never reported, the page says that instead of showing a list it invented.
 */
import { useState } from 'react'
import { motion } from 'framer-motion'
import { RefreshCw, Terminal } from 'lucide-react'
import { toast } from 'sonner'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Switch } from '@/components/ui/switch'

import { ApiError, patchSettings, syncCommands, type CommandRow } from '../../lib/api'
import { useGuild } from '../../lib/guild'
import { SettingsForm } from '../../lib/settings-form'
import { ago, PageHeader, Panel } from './shared'

export default function Commands() {
  const { id, report, settings, reload } = useGuild()
  const [busy, setBusy] = useState(false)
  const listed = report?.commands?.commands ?? []
  const at = report?.commands?.reportedAt
  const doc = settings('commands') as Record<string, unknown>
  const disabled = Array.isArray(doc.disabled_commands) ? (doc.disabled_commands as string[]) : []

  const sync = async () => {
    setBusy(true)
    try {
      await syncCommands(id)
      toast.success('Queued — the bot re-registers them on its next beat.')
      setTimeout(reload, 2500)
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  const toggle = async (name: string, enabled: boolean) => {
    const next = enabled ? disabled.filter((d) => d !== name) : [...disabled, name]
    try {
      await patchSettings(id, 'commands', { disabled_commands: next })
      toast.success(enabled ? `${name} turned on` : `${name} turned off`)
      reload()
    } catch {
      toast.error('Could not save.')
    }
  }

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Commands"
        hint="Guild-scoped, so they appear the moment they are written."
        actions={
          <Button size="sm" variant="outline" onClick={sync} disabled={busy}>
            <motion.span
              animate={busy ? { rotate: 360 } : { rotate: 0 }}
              transition={busy ? { repeat: Infinity, duration: 1, ease: 'linear' } : { duration: 0.2 }}
              className="inline-flex"
            >
              <RefreshCw className="size-4" />
            </motion.span>
            {busy ? 'Queuing…' : 'Sync now'}
          </Button>
        }
      />

      {listed.length === 0 ? (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Nothing reported yet</CardTitle>
            <CardDescription>
              The bot writes this list after its first successful registration. If it has just started, wait a minute and
              reload; if it has been running, press Sync now and the reason will land in its log.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <Button variant="outline" size="sm" onClick={reload}>
              Reload
            </Button>
          </CardContent>
        </Card>
      ) : (
        <Panel
          title={`${listed.length} command${listed.length === 1 ? '' : 's'} in this server`}
          hint={`As the bot last saw them · ${ago(at)}`}
        >
          <ul className="grid gap-1">
            {listed.map((raw) => {
              const c = raw as CommandRow
              const off = disabled.includes(c.name)
              return (
                <li
                  key={c.name}
                  className="flex flex-wrap items-center gap-3 rounded-lg border px-3 py-2"
                >
                  <code className="text-sm font-medium">/{c.name}</code>
                  <span className="me-auto min-w-0 flex-1 truncate text-sm text-muted-foreground" title={c.description}>
                    {c.description || 'no description'}
                  </span>
                  {c.staffOnly ? <Badge variant="outline">staff</Badge> : null}
                  <Switch
                    checked={!off}
                    onCheckedChange={(v) => void toggle(c.name, v)}
                    aria-label={`Enable /${c.name}`}
                  />
                </li>
              )
            })}
          </ul>
          <p className="text-xs text-muted-foreground">
            Turning one off removes it from Discord in this server only. The rest of the product keeps working without it.
          </p>
        </Panel>
      )}

      <SettingsForm
        feature="commands"
        title="Command behaviour"
        icon={Terminal}
        fields={[
          { kind: 'int', key: 'command_cooldown_secs', label: 'One command per member every', hint: 'Stops a member from emptying the queue with /purge.', min: 0, max: 3600, suffix: 'secs' },
          { kind: 'lines', key: 'disabled_channels', label: 'Refuse commands in these channels', hint: 'One channel id per line.' },
        ]}
      />
    </div>
  )
}
