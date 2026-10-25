/**
 * Links and invites, checked locally.
 *
 * This is the cheap half of content moderation: no model, no per-request cost, no data
 * leaving the machine, and a rule nobody can argue with. It runs through the same gate as
 * the text filter, so exemptions, strikes and the mod log behave identically.
 */
import { Badge } from '@/components/ui/badge'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Info, Link2Off } from 'lucide-react'

import { SettingsForm } from '../../lib/settings-form'
import { useGuild } from '../../lib/guild'
import { PageHeader } from './shared'

export default function Links() {
  const { settings } = useGuild()
  const cfg = settings('link_guard') as Record<string, unknown>
  const enabled = Boolean(cfg.enabled ?? true)

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Links & invites"
        hint="Runs on every message, before anything is sent to a model."
        actions={<Badge variant={enabled ? 'default' : 'secondary'}>{enabled ? 'on' : 'off'}</Badge>}
      />

      <div className="grid gap-3 lg:grid-cols-[2fr_1fr]">
        <SettingsForm
          feature="link_guard"
          title="Rules"
          icon={Link2Off}
          tone="warn"
          description="Everything here is evaluated in the gateway worker. Nothing is uploaded."
          fields={[
            { kind: 'switch', key: 'enabled', label: 'Guard on', hint: 'Off means these rules never run.' },
            { kind: 'switch', key: 'block_invites', label: 'Block Discord invites', hint: 'discord.gg, dsc.gg, discord.com/invite and server links.' },
            { kind: 'switch', key: 'block_links', label: 'Block every link', hint: 'http(s):// and www. Only bare words in prose are left alone.' },
            {
              kind: 'lines',
              key: 'allowed_domains',
              label: 'Always allowed',
              hint: 'One domain per line. Matches on domain boundaries: example.com covers docs.example.com, not evil-example.com.',
              placeholder: 'example.com\ndocs.python.org',
            },
            { kind: 'ids', key: 'exempt_role_ids', label: 'Exempt roles', hint: 'Members with any of these roles post links freely.', source: 'roles', multiple: true },
            { kind: 'ids', key: 'exempt_channel_ids', label: 'Exempt channels', hint: 'Media channels, usually.', source: 'channels', multiple: true },
            {
              kind: 'enum',
              key: 'action',
              label: 'When it matches',
              options: [
                { value: 'delete', label: 'Delete' },
                { value: 'delete_warn', label: 'Delete + warn' },
                { value: 'delete_timeout', label: 'Delete + timeout' },
              ],
            },
            { kind: 'int', key: 'timeout_minutes', label: 'Timeout', hint: 'Only used by “Delete + timeout”.', min: 1, max: 40320, suffix: 'minutes' },
          ]}
        />

        <Card>
          <CardHeader>
            <CardTitle className="text-base">How it decides</CardTitle>
          </CardHeader>
          <CardContent className="grid gap-3 text-sm text-muted-foreground">
            <p>Invites are matched first, so allowing a Discord domain never lets an invite through.</p>
            <p>A link is a URL with a scheme, or one that starts with <code className="rounded bg-muted px-1">www.</code>. Filenames and <code className="rounded bg-muted px-1">node.js</code> are safe.</p>
            <p>The message is deleted, the member is told in the mod log, and a strike is added when the action says so.</p>
            <Alert>
              <Info />
              <AlertTitle>Whitelists are shared</AlertTitle>
              <AlertDescription>Roles and channels exempted on the text filter page exempt this too.</AlertDescription>
            </Alert>
          </CardContent>
        </Card>
      </div>
    </div>
  )
}
