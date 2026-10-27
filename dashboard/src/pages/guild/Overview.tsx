/**
 * The guild's front page: what the bot is doing, and what it has recently done.
 *
 * Numbers only. Anything you would configure lives on the page for it, linked from the
 * tiles - an overview that duplicates three forms is three ways to be out of date.
 */
import { motion } from 'framer-motion'
import { Activity, RefreshCw, LifeBuoy, Lock, Pin } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Separator } from '@/components/ui/separator'

import { useGuild } from '../../lib/guild'
import { guildPath } from '../../lib/nav'
import { Link } from '../../router'
import { ago, dateOf, PageHeader, Stat } from './shared'

export default function Overview() {
  const { data, loading, id, reload } = useGuild()
  const mod = (data?.moderation ?? {}) as Record<string, number | null>
  const tickets = data?.tickets ?? {}
  const live = (data?.live ?? null) as Record<string, number | string | null> | null

  const tiles = [
    { label: 'Warns active', value: num(mod.activeWarns ?? mod.active), hint: 'expiring on their own', href: 'moderation' },
    { label: 'Strikes', value: num(mod.strikes ?? mod.strikes), hint: 'towards escalation', href: 'moderation' },
    { label: 'Open tickets', value: num(tickets.open), hint: 'waiting on staff', href: 'tickets' },
    { label: 'Closed, 7 days', value: num(tickets.closedLast7d), hint: 'handled', href: 'tickets' },
  ]

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Overview"
        actions={
          <>
            <Button variant="outline" size="sm" onClick={reload} disabled={loading}>
              <RefreshCw className={loading ? 'animate-spin' : ''} /> Refresh
            </Button>
            <Button  size="sm" variant="ghost" render={<Link href={guildPath(id, 'settings')} />}>Settings</Button>
          </>
        }
      />

      <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-4">
        {tiles.map((t, i) => (
          <motion.div key={t.label} initial={{ opacity: 0, y: 8 }} animate={{ opacity: 1, y: 0 }} transition={{ delay: i * 0.04, duration: 0.26 }}>
            <Link href={guildPath(id, t.href)} className="block">
              <Stat label={t.label} value={loading ? '-' : t.value} hint={t.hint} />
            </Link>
          </motion.div>
        ))}
      </div>

      <div className="grid gap-3 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-base">
              <Activity className="size-4" /> The worker
            </CardTitle>
            <CardDescription>What the gateway process reported last.</CardDescription>
          </CardHeader>
          <CardContent className="grid gap-1 text-sm">
            {live ? (
              <>
                <Row k="Gateway latency" v={`${fmt(live.wsLatencyMs)} ms`} />
                <Row k="Shards ready" v={`${live.shardsReady ?? 0} / ${live.shardsTotal ?? 0}`} />
                <Row k="Messages / min" v={fmt(live.msgsMin ?? live.msgs_min)} />
                <Row k="Queue depth" v={fmt(live.queueDepth)} />
                <Row k="Reported" v={ago(Number(live.reportedAt ?? 0))} />
              </>
            ) : (
              <p className="text-muted-foreground">No report yet. Start the bot worker and it appears within a minute.</p>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-base">
              <Lock className="size-4" /> Guard status
            </CardTitle>
            <CardDescription>Which rules are on right now.</CardDescription>
          </CardHeader>
          <CardContent className="grid gap-2 text-sm">
            <Toggle label="Text filter" on={(data?.aiRules?.enabled ?? 0) > 0 || Boolean((data?.settings?.ai_filter as any)?.enabled)} note={`${data?.aiRules?.total ?? 0} custom rules`} href="filter" />
            <Toggle label="Links & invites" on={Boolean((data?.settings?.link_guard as any)?.enabled)} note="local, no API" href="links" />
            <Toggle label="Anti-spam" on={Boolean((data?.settings?.anti_spam as any)?.enabled)} note="floods, pings, caps" href="antispam" />
            <Separator />
            <div className="flex items-center gap-2">
              <LifeBuoy className="size-4 text-muted-foreground" />
              <span className="me-auto">Raid {data?.raid?.active ? 'in progress' : 'idle'}</span>
              <Badge variant={data?.raid?.active ? 'destructive' : 'secondary'}>{data?.raid?.active ? 'locked' : 'ready'}</Badge>
              <Button  size="sm" variant="outline" render={<Link href={guildPath(id, 'raid')} />}>Open</Button>
            </div>
            <div className="flex items-center gap-2">
              <Pin className="size-4 text-muted-foreground" />
              <span className="me-auto">Stickies armed</span>
              <Badge variant="outline">{(data?.settings?.sticky as any)?.enabled === false ? 'off' : 'on'}</Badge>
              <Button  size="sm" variant="outline" render={<Link href={guildPath(id, 'stickies')} />}>Open</Button>
            </div>
            <p className="pt-1 text-xs text-muted-foreground">
              Added {dateOf(data?.guild?.addedAt)} · last synced {ago(data?.guild?.syncedAt)}
            </p>
          </CardContent>
        </Card>
      </div>
    </div>
  )
}

function Row({ k, v }: { k: string; v: string }) {
  return (
    <div className="flex items-baseline gap-2">
      <span className="text-muted-foreground">{k}</span>
      <span className="ms-auto tabular-nums">{v}</span>
    </div>
  )
}

function Toggle({ label, on, note, href }: { label: string; on: boolean; note: string; href: string }) {
  const { id } = useGuild()
  return (
    <div className="flex items-center gap-2">
      <span className={`size-2 rounded-full ${on ? 'bg-primary' : 'bg-muted-foreground/40'}`} />
      <span className="me-auto">
        {label} <span className="text-muted-foreground">· {note}</span>
      </span>
      <Button  size="sm" variant="ghost" render={<Link href={guildPath(id, href)} />}>Tune</Button>
    </div>
  )
}

const num = (v: unknown): number => (typeof v === 'number' ? v : Number(v ?? 0) || 0)
const fmt = (v: unknown): string => (typeof v === 'number' ? String(Math.round(v)) : String(v ?? '-'))
