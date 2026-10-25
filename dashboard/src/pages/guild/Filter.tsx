/**
 * The text filter: your own rules first, the model second.
 *
 * Custom rules are matched in the same pass as everything else in this file — locally, free,
 * before anything is sent to a provider. The model only runs for what the guild asks it to
 * look at, and the box at the bottom calls the very function the gate calls, so a preview here
 * is the real verdict rather than an approximation of it.
 */
import { useState } from 'react'
import { toast } from 'sonner'
import { Ban, ScanEye, Search, Trash2 } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { TableCell } from '@/components/ui/table'
import { Switch } from '@/components/ui/switch'

import { ApiError, deleteRule, patchRule, putRule, rules as fetchRules, testFilter, type FilterTest, type Rule } from '../../lib/api'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { SettingsForm } from '../../lib/settings-form'
import { PageHeader, Panel, Table } from './shared'

export default function Filter() {
  const { id } = useGuild()
  const list = useAsync((signal) => fetchRules(id), [id])
  const rows = (list.data?.rules ?? []) as Rule[]
  const [pattern, setPattern] = useState('')
  const [busy, setBusy] = useState(false)

  const add = async () => {
    if (pattern.trim().length < 2) {
      toast.error('A rule needs at least two characters.')
      return
    }
    setBusy(true)
    try {
      await putRule(id, { pattern: pattern.trim(), mode: 'substring', action: 'delete', severity: 'medium', category: 'custom', enabled: true })
      toast.success('Rule added')
      setPattern('')
      list.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  const toggle = async (r: Rule, enabled: boolean) => {
    try {
      await patchRule(id, r.id, enabled)
      list.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'Could not change the rule.')
    }
  }

  const drop = async (r: Rule) => {
    try {
      await deleteRule(id, r.id)
      toast.success('Rule removed')
      list.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'Could not remove the rule.')
    }
  }

  return (
    <div className="grid gap-5">
      <PageHeader title="Text filter" hint="Words and patterns you decide, then a model for the grey areas." />

      <Panel title="Custom rules" hint="Checked on every message, in this process. Nothing is sent anywhere.">
        <div className="flex flex-wrap items-center gap-2">
          <Input
            className="max-w-[36ch]"
            value={pattern}
            onChange={(e) => setPattern(e.target.value)}
            placeholder="a word or a /regex/"
            onKeyDown={(e) => {
              if (e.key === 'Enter') void add()
            }}
          />
          <Button onClick={add} disabled={busy}>
            Add rule
          </Button>
          <span className="text-xs text-muted-foreground">Deletes the message and logs it. Change the details below.</span>
        </div>
        <Table
          head={['Pattern', 'Mode', 'Action', 'Severity', 'Hits', 'On', '']}
          rows={rows}
          render={(raw) => {
            const r = raw as Rule
            return (
              <>
                <TableCell className="max-w-[30ch] truncate font-medium" title={r.pattern}>
                  <code className="text-[13px]">{r.pattern}</code>
                </TableCell>
                <TableCell className="text-muted-foreground">{r.mode}</TableCell>
                <TableCell>
                  <Badge variant={r.action === 'flag' ? 'outline' : 'secondary'}>{r.action.replace('_', ' ')}</Badge>
                </TableCell>
                <TableCell className="text-muted-foreground">{r.severity}</TableCell>
                <TableCell className="tabular-nums">{r.hits}</TableCell>
                <TableCell>
                  <Switch checked={r.enabled} onCheckedChange={(v) => void toggle(r, v)} aria-label={`Enable ${r.pattern}`} />
                </TableCell>
                <TableCell className="text-end">
                  <Button variant="ghost" size="sm" onClick={() => void drop(r)}>
                    <Trash2 className="size-4" />
                  </Button>
                </TableCell>
              </>
            )
          }}
        />
      </Panel>

      <TestBox guild={id} />

      <SettingsForm
        feature="ai_filter"
        title="Detection"
        icon={ScanEye}
        description="Rules run first. The model only sees what this server asks it to."
        fields={[
          { kind: 'switch', key: 'enabled', label: 'Send messages for classification', hint: 'Needs a provider key on this install. Rules keep working without it.' },
          {
            kind: 'enum',
            key: 'mode',
            label: 'What decides',
            options: [
              { value: 'rules', label: 'My rules only' },
              { value: 'ml', label: 'The model only' },
              { value: 'rules,ml', label: 'Both' },
            ],
          },
          {
            kind: 'enum',
            key: 'min_severity',
            label: 'Act from severity',
            hint: 'Anything below this is logged and left alone.',
            options: [
              { value: 'info', label: 'Info' },
              { value: 'low', label: 'Low' },
              { value: 'medium', label: 'Medium' },
              { value: 'high', label: 'High' },
              { value: 'critical', label: 'Critical' },
            ],
          },
          { kind: 'int', key: 'strictness', label: 'Sensitivity', hint: '−5 relaxed, +5 paranoid. Moves every class at once.', min: -5, max: 5 },
          { kind: 'int', key: 'min_chars', label: 'Ignore messages shorter than', hint: 'A cost control for the model, nothing else.', min: 0, max: 500, suffix: 'chars' },
          { kind: 'int', key: 'max_chars', label: 'Cut off checking at', min: 100, max: 4000, suffix: 'chars' },
          { kind: 'int', key: 'cache_seconds', label: 'Remember a verdict for', hint: '0 checks every time.', min: 0, max: 3600, suffix: 'secs' },
          { kind: 'switch', key: 'apply_to_edits', label: 'Re-check edited messages' },
          { kind: 'switch', key: 'apply_to_embeds', label: 'Count link embeds as content' },
          { kind: 'switch', key: 'apply_to_bots', label: 'Check other bots', hint: 'Off means a bot posting abuse slips through — your call.' },
          {
            kind: 'enum',
            key: 'on_api_error',
            label: 'If the provider is down',
            options: [
              { value: 'allow', label: 'Let messages through' },
              { value: 'block', label: 'Delete them' },
            ],
          },
          { kind: 'switch', key: 'privacy_redact', label: 'Store only the matched part', hint: 'Off keeps the whole message in the audit row.' },
        ]}
      />

      <SettingsForm
        feature="ai_filter"
        title="What happens on a hit"
        icon={Ban}
        tone="warn"
        fields={[
          { kind: 'switch', key: 'auto_delete', label: 'Delete the message' },
          { kind: 'switch', key: 'dm_user', label: 'DM the member the reason' },
          { kind: 'switch', key: 'warn_on_hit', label: 'Add a warn' },
          { kind: 'switch', key: 'strikes_enabled', label: 'Count strikes' },
          { kind: 'int', key: 'strikes_max', label: 'Escalate after', hint: 'Uses the timeout, kick or ban set on the Moderation page.', min: 1, max: 20, suffix: 'strikes' },
          { kind: 'int', key: 'strike_expiry_days', label: 'A strike forgets after', min: 1, max: 365, suffix: 'days' },
          { kind: 'int', key: 'timeout_minutes', label: 'Timeout length', hint: 'Used when escalation is a timeout.', min: 1, max: 40320, suffix: 'mins' },
          { kind: 'switch', key: 'ban_on_strikes', label: 'Ban instead of timing out' },
          { kind: 'ids', key: 'whitelist_role_ids', label: 'Never check these roles', source: 'roles', multiple: true },
          { kind: 'ids', key: 'whitelist_channel_ids', label: 'Never check these channels', source: 'channels', multiple: true },
          { kind: 'lines', key: 'whitelist_user_ids', label: 'Never check these members', hint: 'One user id per line — Discord shows it when you copy an id.' },
        ]}
      />
    </div>
  )
}

/** The one place in the panel that calls the same function the gateway calls. It is a card
 *  rather than a modal because the answer is worth reading twice. */
function TestBox({ guild }: { guild: string }) {
  const [text, setText] = useState('')
  const [out, setOut] = useState<FilterTest | null>(null)
  const [busy, setBusy] = useState(false)

  const run = async () => {
    if (!text.trim()) return
    setBusy(true)
    try {
      setOut(await testFilter(guild, text))
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Try a sentence</CardTitle>
        <CardDescription>Same code path as a real message. Nothing is posted, nothing is deleted.</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-3">
        <Label className="grid gap-1.5">
          <span className="sr-only">Text to check</span>
          <Input value={text} onChange={(e) => setText(e.target.value)} placeholder="What somebody might post…" onKeyDown={(e) => e.key === 'Enter' && void run()} />
        </Label>
        <div className="flex items-center gap-2">
          <Button size="sm" onClick={run} disabled={busy}>
            <Search className="size-4" /> Check it
          </Button>
          {out ? (
            <Badge variant={out.blocked ? 'destructive' : 'outline'}>{out.blocked ? out.verdict.action.replace('_', ' ') : 'allowed'}</Badge>
          ) : null}
        </div>
        {out ? (
          <dl className="grid gap-1 text-sm">
            <Row k="Why" v={out.verdict.reason || 'nothing matched'} />
            <Row k="Severity" v={out.verdict.severity} />
            <Row k="Classes" v={out.verdict.categories.length ? out.verdict.categories.join(', ') : '—'} />
            <Row k="Provider" v={out.provider.called ? 'asked' : (out.provider.note ?? 'not needed')} />
            <Row k="Rules compared" v={String(out.rulesCompared)} />
          </dl>
        ) : null}
      </CardContent>
    </Card>
  )
}

function Row({ k, v }: { k: string; v: string }) {
  return (
    <div className="flex gap-3">
      <dt className="w-32 shrink-0 text-muted-foreground">{k}</dt>
      <dd className="min-w-0 break-words">{v}</dd>
    </div>
  )
}
