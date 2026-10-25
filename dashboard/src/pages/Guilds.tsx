/**
 * The first page: every server this Discord account administers.
 *
 * Aerona's own servers come first, because they are the ones with data. The rest are not
 * dead links — each carries the invite the API built for it — and they stop being "the rest"
 * the moment the bot lands in one.
 */
import { useMemo, useState } from 'react'
import { motion } from 'framer-motion'
import { ArrowRight, UserPlus } from 'lucide-react'

import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty'
import { Input } from '@/components/ui/input'
import { Skeleton } from '@/components/ui/skeleton'

import { ApiError, guilds as fetchGuilds, type GuildSummary } from '../lib/api'
import { useAsync } from '../lib/hooks'
import { guildPath } from '../lib/nav'
import { Link } from '../router'
import { ThemeToggle } from '../components/theme-toggle'
import { Brand } from '../components/brand'
import { guildInitials, iconUrl } from './guild/shared'

export default function Guilds() {
  const [query, setQuery] = useState('')
  const { data, error, loading } = useAsync<{ guilds: GuildSummary[] }>((signal) => fetchGuilds(), [])

  const rows = useMemo(() => {
    const all = data?.guilds ?? []
    const q = query.trim().toLowerCase()
    const hits = q ? all.filter((g) => g.name.toLowerCase().includes(q)) : all
    // Presence first, then name: the server you are here to look at should be the first row.
    return [...hits].sort((a, b) => Number(b.botPresent) - Number(a.botPresent) || a.name.localeCompare(b.name))
  }, [data, query])

  const here = rows.filter((g) => g.botPresent)
  const away = rows.filter((g) => !g.botPresent)

  return (
    <div className="min-h-svh bg-muted/30">
      <header className="border-b bg-background/70 backdrop-blur">
        <div className="mx-auto flex max-w-4xl items-center gap-3 px-5 py-4">
          <Brand size={32} />
          <p className="me-auto text-sm text-muted-foreground">Pick a server.</p>
          <ThemeToggle />
        </div>
      </header>

      <main id="main" className="mx-auto grid max-w-4xl gap-6 px-5 py-8">
        <div className="flex items-center gap-3">
          <Input
            placeholder="Search servers"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            className="max-w-xs"
          />
          <p className="ms-auto text-sm text-muted-foreground">
            {here.length} set up · {away.length} to add
          </p>
        </div>

        {loading ? (
          <div className="grid gap-2">
            {[0, 1, 2].map((i) => (
              <Skeleton key={i} className="h-20 w-full rounded-xl" />
            ))}
          </div>
        ) : error ? (
          <Card>
            <CardContent className="p-6">
              <p className="font-medium">The API did not answer.</p>
              <p className="text-sm text-muted-foreground">
                {error instanceof ApiError ? error.message : 'Is the server running?'}
              </p>
            </CardContent>
          </Card>
        ) : rows.length === 0 ? (
          <Empty className="border">
            <EmptyHeader>
              <EmptyMedia variant="icon">
                <UserPlus />
              </EmptyMedia>
              <EmptyTitle>No servers yet</EmptyTitle>
              <EmptyDescription>
                Sign in with a Discord account that manages a server, then add Aerona to it.
              </EmptyDescription>
            </EmptyHeader>
          </Empty>
        ) : (
          <>
            {here.length ? <Section title="Aerona is here" rows={here} /> : null}
            {away.length ? <Section title="Add Aerona" rows={away} /> : null}
          </>
        )}
      </main>
    </div>
  )
}

function Section({ title, rows }: { title: string; rows: GuildSummary[] }) {
  return (
    <section className="grid gap-2">
      <h2 className="text-xs font-medium text-muted-foreground uppercase tracking-wide">{title}</h2>
      {rows.map((g, i) => (
        <motion.div
          key={g.id}
          initial={{ opacity: 0, y: 6 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ delay: Math.min(i, 8) * 0.03, duration: 0.24, ease: [0.22, 1, 0.36, 1] }}
        >
          <Link href={guildPath(g.id, '')} className="group block">
            <Card className="transition-colors hover:bg-accent/40">
              <CardContent className="flex items-center gap-3 p-4">
                <Avatar className="size-10 rounded-xl">
                  <AvatarImage src={iconUrl(g.id, g.icon) ?? undefined} alt="" />
                  <AvatarFallback className="rounded-xl text-xs">{guildInitials(g.name)}</AvatarFallback>
                </Avatar>
                <div className="me-auto min-w-0 leading-tight">
                  <p className="truncate font-medium">{g.name}</p>
                  <p className="text-sm text-muted-foreground">
                    {g.memberCount ? `${g.memberCount.toLocaleString()} members` : 'Members unknown'}
                    {g.botPresent && g.openTickets ? ` · ${g.openTickets} open tickets` : ''}
                    {g.botPresent && g.stickies ? ` · ${g.stickies} stickies` : ''}
                  </p>
                </div>
                {g.botPresent ? (
                  <>
                    <Badge variant="outline">{g.stickies ?? 0} stickies</Badge>
                    <Button size="sm" variant="ghost" className="gap-1">
                      Open <ArrowRight className="size-4" />
                    </Button>
                  </>
                ) : (
                  g.inviteUrl ? (
                    <a href={g.inviteUrl} target="_blank" rel="noreferrer" onClick={(e) => e.stopPropagation()}>
                      <Button size="sm">
                        <UserPlus className="size-4" /> Add to Discord
                      </Button>
                    </a>
                  ) : (
                    <Badge variant="secondary">not installed</Badge>
                  )
                )}
              </CardContent>
            </Card>
          </Link>
        </motion.div>
      ))}
    </section>
  )
}
