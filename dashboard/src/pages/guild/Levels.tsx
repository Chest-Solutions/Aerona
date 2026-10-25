/**
 * XP and ranks.
 *
 * The bot adds one point per message and levels on a curve it keeps in the database, so this
 * page is a scoreboard and a switch — there is no rate to tune that would not make the numbers
 * mean something other than what they mean.
 */
import { Medal } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Progress } from '@/components/ui/progress'
import { TableCell } from '@/components/ui/table'

import { levels as fetchLevels, type LevelRow } from '../../lib/api'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { SettingsForm } from '../../lib/settings-form'
import { PageHeader, Panel, shortId, Table } from './shared'

export default function Levels() {
  const { id } = useGuild()
  const list = useAsync((signal) => fetchLevels(id, 25), [id])
  const rows = (list.data?.leaderboard ?? []) as LevelRow[]
  const curve = list.data?.curve?.xpForLevel ?? []
  const top = rows[0]?.xp ?? 1

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Levels"
        hint="One point per message. That is the whole economy."
        actions={list.data?.enabled ? <Badge variant="secondary">{rows.length} ranked</Badge> : <Badge variant="outline">off</Badge>}
      />

      <SettingsForm
        feature="general"
        title="Ranks"
        icon={Medal}
        description="Switching this off stops the counter, not the leaderboard: what is already earned stays."
        fields={[
          { kind: 'switch', key: 'levels_enabled', label: 'Count messages towards a level' },
          { kind: 'switch', key: 'dms_enabled', label: 'Let members use commands in DMs' },
        ]}
      />

      <Panel title="Leaderboard" hint={list.loading ? 'Loading…' : `Top ${rows.length} in this server`}>
        <Table
          head={['#', 'Member', 'Level', 'XP', 'Messages', 'To next']}
          rows={rows}
          render={(raw, i) => {
            const r = raw as LevelRow
            const next = curve[Math.min(r.level, Math.max(0, curve.length - 1))] ?? 0
            const prev = r.level > 1 ? curve[r.level - 2] ?? 0 : 0
            const pct = next > prev ? Math.min(100, Math.round(((r.xp - prev) / (next - prev)) * 100)) : 0
            return (
              <>
                <TableCell className="tabular-nums text-muted-foreground">{i + 1}</TableCell>
                <TableCell className="font-medium">
                  {r.name}
                  <span className="ms-2 text-xs text-muted-foreground" title={r.userId}>
                    {shortId(r.userId)}
                  </span>
                </TableCell>
                <TableCell>
                  <Badge variant="outline">lv {r.level}</Badge>
                </TableCell>
                <TableCell className="tabular-nums">{r.xp.toLocaleString()}</TableCell>
                <TableCell className="tabular-nums text-muted-foreground">{r.messages.toLocaleString()}</TableCell>
                <TableCell className="w-40">
                  <Progress value={pct} aria-label={`${pct}% to level ${r.level + 1}`} />
                </TableCell>
              </>
            )
          }}
        />
      </Panel>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">The curve</CardTitle>
          <CardDescription>XP each level costs, as the bot computes it.</CardDescription>
        </CardHeader>
        <CardContent className="flex flex-wrap gap-2">
          {curve.length === 0 ? (
            <span className="text-sm text-muted-foreground">Nothing reported yet.</span>
          ) : (
            curve.map((xp, i) => (
              <Badge key={i} variant="outline" className="tabular-nums">
                {i + 1} · {xp.toLocaleString()}
              </Badge>
            ))
          )}
          {top > 0 ? null : <span className="text-sm text-muted-foreground">No points earned in this server yet.</span>}
        </CardContent>
      </Card>
    </div>
  )
}
