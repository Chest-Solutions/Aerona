/**
 * Rate and shape limits. The counters live in the worker's memory, so these rules cost
 * nothing and reset when it restarts - which is why the flood limit is off by default:
 * a limit that is too tight silences an announcement, and the operator hears about it
 * from their members, not from this page.
 */
import { Gauge, Shapes, Zap } from 'lucide-react'
import { Badge } from '@/components/ui/badge'

import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { SettingsForm } from '../../lib/settings-form'
import { useGuild } from '../../lib/guild'
import { PageHeader } from './shared'

export default function AntiSpam() {
  const { settings } = useGuild()
  const cfg = settings('anti_spam') as Record<string, unknown>

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Anti-spam"
        actions={<Badge variant={cfg.enabled ? 'default' : 'secondary'}>{cfg.enabled ? 'on' : 'off'}</Badge>}
      />

      <div className="grid gap-3 lg:grid-cols-2">
        <SettingsForm
          feature="anti_spam"
          title="Flood"
          icon={Gauge}
          fields={[
            { kind: 'switch', key: 'enabled', label: 'Guard on' },
            { kind: 'int', key: 'max_messages', label: 'Messages', hint: '0 turns this rule off.', min: 0, max: 50, suffix: `in the window` },
            { kind: 'int', key: 'window_secs', label: 'Window', min: 1, max: 600, suffix: 'seconds' },
            { kind: 'int', key: 'min_chars', label: 'Ignore short messages', hint: 'So “wow” and “+” never count.', min: 0, max: 200, suffix: 'characters' },
          ]}
        />

        <SettingsForm
          feature="anti_spam"
          title="Shape"
          icon={Shapes}
          fields={[
            { kind: 'int', key: 'max_mentions', label: 'Mentions', hint: 'Users, roles, @everyone, @here. 0 turns it off.', min: 0, max: 100 },
            { kind: 'int', key: 'max_emoji', label: 'Emoji', hint: 'Runs count once, so a family is one.', min: 0, max: 100 },
            { kind: 'int', key: 'max_caps_percent', label: 'Capital letters', hint: 'Percent of the letters in the message. 0 turns it off.', min: 0, max: 100, suffix: '%' },
          ]}
        />
      </div>

      <SettingsForm
        feature="anti_spam"
        title="When a rule fires"
        icon={Zap}
        tone="warn"
        fields={[
          {
            kind: 'enum',
            key: 'action',
            label: 'Action',
            options: [
              { value: 'delete', label: 'Delete' },
              { value: 'delete_warn', label: 'Delete + warn' },
              { value: 'delete_timeout', label: 'Delete + timeout' },
            ],
          },
          { kind: 'int', key: 'timeout_minutes', label: 'Timeout', min: 1, max: 40320, suffix: 'minutes' },
          { kind: 'ids', key: 'exempt_role_ids', label: 'Exempt roles', source: 'roles', multiple: true },
          { kind: 'ids', key: 'exempt_channel_ids', label: 'Exempt channels', hint: 'Bot-spam channels and post threads.', source: 'channels', multiple: true },
        ]}
      />
    </div>
  )
}
