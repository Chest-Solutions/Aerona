/**
 * The pieces every guild page needs, in one file so they stay one visual language:
 * a header, a stat tile, a "the bot isn't here" state, and the time formatting that
 * keeps "3 min ago" identical everywhere instead of re-invented per page.
 */
import { motion } from 'framer-motion'
import type { LucideIcon } from 'lucide-react'
import { Bot, TrendingDown, TrendingUp } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty'
import { Skeleton } from '@/components/ui/skeleton'
import { Table as ShadTable, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'

import { useRouter } from '../../router'
import { pageFor } from '../../lib/nav'
import { useSession } from '../../lib/hooks'

/**
 * The page's own head. Its glyph comes from `lib/nav.ts` unless a page overrides it, so the mark in
 * the rail and the mark above the content are the same for the same page — one table, two places,
 * no chance to disagree, and no page able to arrive unmarked.
 */
export function PageHeader({
  title,
  hint,
  actions,
  icon,
}: {
  title: string
  hint?: string
  actions?: React.ReactNode
  icon?: LucideIcon
}) {
  const { params } = useRouter()
  const Icon = icon ?? pageFor(params.page ?? '')?.icon
  return (
    <div className="flex flex-wrap items-end gap-3 pb-1">
      {Icon ? (
        <span className="grid size-10 shrink-0 place-items-center rounded-xl border bg-muted/40 text-muted-foreground" aria-hidden>
          <Icon className="size-5" />
        </span>
      ) : null}
      <div className="me-auto leading-tight">
        <h1 className="text-xl font-semibold tracking-tight">{title}</h1>
        {hint ? <p className="text-sm text-muted-foreground">{hint}</p> : null}
      </div>
      {actions ? <div className="flex items-center gap-2">{actions}</div> : null}
    </div>
  )
}

export function Stat({
  label,
  value,
  delta,
  hint,
}: {
  label: string
  value: string | number
  delta?: number | null
  hint?: string
}) {
  const up = typeof delta === 'number' && delta > 0
  const down = typeof delta === 'number' && delta < 0
  return (
    <Card>
      <CardHeader className="pb-2">
        <CardDescription className="text-xs font-medium uppercase tracking-wide">{label}</CardDescription>
        <CardTitle className="text-2xl tabular-nums font-semibold">{value}</CardTitle>
      </CardHeader>
      <CardContent className="flex items-center gap-2 text-sm">
        {delta === undefined || delta === null ? null : (
          <Badge variant={up ? 'outline' : down ? 'destructive' : 'secondary'} className="gap-1">
            {up ? <TrendingUp className="size-3" /> : null}
            {down ? <TrendingDown className="size-3" /> : null}
            {delta > 0 ? `+${delta}` : delta}
          </Badge>
        )}
        {hint ? <span className="text-muted-foreground">{hint}</span> : null}
      </CardContent>
    </Card>
  )
}

export function Loading() {
  return (
    <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-4">
      {[0, 1, 2, 3].map((i) => (
        <Skeleton key={i} className="h-28 rounded-xl" />
      ))}
    </div>
  )
}

/**
 * The state a guild page shows when Aerona is not in that server. It is not an error page:
 * the URL is right, the operator is right, only the bot is missing — so the whole screen is
 * one button that fixes it, and Discord sends them back here afterwards.
 */
export function BotNotHere({ guild, name, inviteUrl }: { guild: string; name: string; inviteUrl: string | null }) {
  const { navigate } = useRouter()
  const { me } = useSession()
  return (
    <motion.div initial={{ opacity: 0, y: 8 }} animate={{ opacity: 1, y: 0 }} transition={{ duration: 0.28 }}>
      <Empty className="min-h-[60svh] border-0">
        <EmptyHeader>
          <EmptyMedia variant="icon">
            <Bot />
          </EmptyMedia>
          <EmptyTitle>Aerona isn’t in {name} yet</EmptyTitle>
          <EmptyDescription>
            Add the bot, then come straight back to this page. Nothing here changes until you do.
          </EmptyDescription>
        </EmptyHeader>
        <EmptyContent className="flex flex-wrap justify-center gap-2">
          {inviteUrl ? (
            <a href={inviteUrl} target="_blank" rel="noreferrer">
              <Button>Add Aerona to this server</Button>
            </a>
          ) : (
            <p className="text-sm text-muted-foreground">
              This account can’t invite to this server. Guild id {guild}.
            </p>
          )}
          <Button variant="outline" onClick={() => navigate('/')}>
            Back to servers
          </Button>
          {me?.kind === 'key' ? (
            <p className="text-xs text-muted-foreground">Signed in with an API key, so the invite link is not yours to use.</p>
          ) : null}
        </EmptyContent>
      </Empty>
    </motion.div>
  )
}

export function iconUrl(guildId: string, hash: string | null | undefined, size = 64): string | null {
  if (!hash) return null
  const ext = hash.startsWith('a_') ? 'gif' : 'png'
  return `https://cdn.discordapp.com/icons/${guildId}/${hash}.${ext}?size=${size}`
}

export function guildInitials(name: string): string {
  return name
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((w) => w[0]!.toUpperCase())
    .join('')
}

/**
 * "just now" / "4 min ago" / "3 h ago" / "12 d ago" — the only time format in the panel, and
 * it owns the word "ago". Leaving that to each caller is what made "never ago" and "just now
 * ago" possible, and a report row's age is the one number these pages have to get right.
 */
export function ago(epochSecs: number | null | undefined): string {
  if (!epochSecs) return 'never'
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - epochSecs)
  if (secs < 45) return 'just now'
  const mins = Math.floor(secs / 60)
  if (mins < 60) return `${mins} min ago`
  const hours = Math.floor(mins / 60)
  if (hours < 36) return `${hours} h ago`
  return `${Math.floor(hours / 24)} d ago`
}

export function dateOf(epochSecs: number | null | undefined): string {
  if (!epochSecs) return '—'
  return new Date(epochSecs * 1000).toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' })
}

/** `123456789012345678` → `1234…5678`. An id is never a mystery in this panel: the full
 *  value is the element's title, so a hover or a select copies the whole thing. */
export function shortId(id: string | number | null | undefined): string {
  const v = String(id ?? '')
  if (!v) return '—'
  return v.length > 10 ? `${v.slice(0, 4)}…${v.slice(-4)}` : v
}

/** One card with a title, a one-line hint and a right-aligned action: the shape every
 *  table, list and log in this panel uses, so a page never re-invents its own header. */
export function Panel({
  title,
  hint,
  actions,
  children,
  className,
}: {
  title: string
  hint?: string
  actions?: React.ReactNode
  children: React.ReactNode
  className?: string
}) {
  return (
    <Card className={className}>
      <CardHeader className="flex-row items-start gap-3 space-y-0">
        <div className="grid gap-1 me-auto">
          <CardTitle className="text-base">{title}</CardTitle>
          {hint ? <CardDescription>{hint}</CardDescription> : null}
        </div>
        {actions ? <div className="flex items-center gap-2">{actions}</div> : null}
      </CardHeader>
      <CardContent className="grid gap-3">{children}</CardContent>
    </Card>
  )
}

/** A table that knows what an empty list should say, because "no rows" is the state most
 *  operators actually land on and it should point at the thing that fills it. */
export function Table({ head, rows, render }: { head: string[]; rows: readonly unknown[]; render: (row: any, i: number) => React.ReactNode }) {
  return (
    <ScrollTable>
      <ShadTable>
        <TableHeader>
          <TableRow>
            {head.map((h) => (
              <TableHead key={h}>{h}</TableHead>
            ))}
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.length === 0 ? (
            <TableRow>
              <TableCell colSpan={head.length} className="text-sm text-muted-foreground">
                Nothing here yet.
              </TableCell>
            </TableRow>
          ) : (
            rows.map((r, i) => <TableRow key={i}>{render(r, i)}</TableRow>)
          )}
        </TableBody>
      </ShadTable>
    </ScrollTable>
  )
}

function ScrollTable({ children }: { children: React.ReactNode }) {
  return <div className="overflow-x-auto">{children}</div>
}
