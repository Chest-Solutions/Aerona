/**
 * Every page inside one server.
 *
 * The rail on the left is the product's feature list, read from `lib/nav.ts` - one table, so
 * the sidebar, the routes and the section heads can never disagree about what exists. The
 * guard at the top is the other half: no session sends you to Discord with this URL attached,
 * and no bot in the server shows you the invite instead of an empty moderation table.
 */
import { useMemo, useState } from 'react'
import { AnimatePresence, motion, useReducedMotion } from 'framer-motion'
import { ChevronDown, Check, List, ShieldCheck } from 'lucide-react'

import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger } from '@/components/ui/dropdown-menu'
import { Sidebar, SidebarContent, SidebarFooter, SidebarGroup, SidebarGroupContent, SidebarGroupLabel, SidebarHeader, SidebarInset, SidebarMenu, SidebarMenuButton, SidebarMenuItem, SidebarProvider, SidebarTrigger } from '@/components/ui/sidebar'
import { Skeleton } from '@/components/ui/skeleton'

import { guilds as fetchGuilds } from '../lib/api'
import { GuildProvider, useGuild } from '../lib/guild'
import { guildGroups, guildPages, guildPath, pageFor, type GuildPage } from '../lib/nav'
import { useAsync } from '../lib/hooks'
import { Link, useRouter } from '../router'
import { ThemeToggle } from '../components/theme-toggle'
import { BotNotHere, guildInitials, iconUrl, Loading } from './guild/shared'

import Overview from './guild/Overview'
import Moderation from './guild/Moderation'
import Tickets from './guild/Tickets'
import Stickies from './guild/Stickies'
import Capes from './guild/Capes'
import Raid from './guild/Raid'
import Filter from './guild/Filter'
import Links from './guild/Links'
import AntiSpam from './guild/AntiSpam'
import Welcome from './guild/Welcome'
import Levels from './guild/Levels'
import Logs from './guild/Logs'
import Commands from './guild/Commands'
import Profile from './guild/Profile'
import Settings from './guild/Settings'
import Audit from './guild/Audit'

const VIEWS: Record<string, React.ComponentType> = {
  '': Overview,
  moderation: Moderation,
  tickets: Tickets,
  stickies: Stickies,
  capes: Capes,
  raid: Raid,
  filter: Filter,
  links: Links,
  antispam: AntiSpam,
  welcome: Welcome,
  levels: Levels,
  logs: Logs,
  commands: Commands,
  profile: Profile,
  settings: Settings,
  audit: Audit,
}

export default function GuildApp({ params }: { params: Record<string, string> }) {
  const guild = params.guild ?? ''
  const page = params.page ?? ''
  return (
    <GuildProvider id={guild} page={page}>
      <Shell page={page} />
    </GuildProvider>
  )
}

function Shell({ page }: { page: string }) {
  const { id, name, loading, error, present, inviteUrl } = useGuild()
  const { navigate } = useRouter()

  // There is no sign-in logic here any more on purpose: `AuthGate` in App.tsx is the one place
  // a browser without a session is turned toward /login/ with this URL attached as `?next=`, so
  // the callback can bring it back to the page it actually wanted. What is left to decide here
  // is what a *signed-in* visitor sees when the answer about a server is bad.
  // A guild the API does not know is not "missing the bot": it is a wrong id, and saying so
  // beats offering an invite for a server nobody can add a bot to.
  if (error && !loading) {
    return (
      <Frame>
        <div className="rounded-xl border p-6">
          <p className="font-medium">Nothing is configured for that server id.</p>
          <p className="text-sm text-muted-foreground">
            {error.code === 'not_found' ? `Aerona has never seen ${id}.` : error.message}
          </p>
          <Button variant="outline" size="sm" className="mt-3" onClick={() => navigate('/')}>
            All servers
          </Button>
        </div>
      </Frame>
    )
  }
  if (loading && !present) return <Frame skeleton />
  if (!present) return <Frame>{<BotNotHere guild={id} name={name} inviteUrl={inviteUrl} />}</Frame>

  return <Body page={page} />
}

function Body({ page }: { page: string }) {
  const { id } = useGuild()
  const def = pageFor(page)
  const View = VIEWS[page]
  return (
    <Frame>
      <div className="grid gap-5">
        {!def ? (
          <div className="rounded-xl border p-6">
            <p className="font-medium">There is no “{page}” in this server.</p>
            <Button variant="outline" size="sm" className="mt-3" render={<Link href={guildPath(id, '')} />}>
              Back to overview
            </Button>
          </div>
        ) : (
          <AnimatePresence mode="wait" initial={false}>
            <motion.div
              key={page || 'overview'}
              initial={{ opacity: 0, y: 8 }}
              animate={{ opacity: 1, y: 0 }}
              exit={{ opacity: 0, y: -4 }}
              transition={{ duration: 0.2, ease: [0.22, 1, 0.36, 1] }}
            >
              {View ? <View /> : <Loading />}
            </motion.div>
          </AnimatePresence>
        )}
      </div>
    </Frame>
  )
}

/** The chrome: rail on the left, one line of context on top, content in the middle. */
function Frame({ children, skeleton }: { children?: React.ReactNode; skeleton?: boolean }) {
  return (
    <SidebarProvider className="min-h-svh">
      <Rail />
      <SidebarInset>
        <TopBar skeleton={skeleton} />
        <div className="mx-auto w-full max-w-6xl px-5 py-6">{children}</div>
      </SidebarInset>
    </SidebarProvider>
  )
}

function Rail() {
  // Read once, at the top: a hook inside the `.map` below would be a different number of
  // hooks per render, which React forbids and the production build catches later and louder.
  const { id, name, icon, present, page } = useGuild()
  const active = pageFor(page)?.seg ?? ''
  return (
    <Sidebar collapsible="icon">
      <SidebarHeader>
        <GuildSwitcher current={{ id, name, icon, botPresent: present }} />
      </SidebarHeader>
      <SidebarContent>
        {guildGroups.map((group) => {
          const rows = guildPages.filter((p) => p.group === group)
          if (!rows.length) return null
          return (
            <SidebarGroup key={group}>
              <SidebarGroupLabel>{group}</SidebarGroupLabel>
              <SidebarGroupContent>
                <SidebarMenu>
                  {rows.map((p) => (
                    <SidebarMenuItem key={p.seg || 'overview'}>
                      <SidebarMenuButton
                        isActive={active === p.seg}
                        tooltip={p.label}
                        render={<Link href={guildPath(id, p.seg)} />}
                      >
                        <p.icon />
                        <span>{p.label}</span>
                      </SidebarMenuButton>
                    </SidebarMenuItem>
                  ))}
                </SidebarMenu>
              </SidebarGroupContent>
            </SidebarGroup>
          )
        })}
        <SidebarGroup>
          <SidebarGroupLabel>This install</SidebarGroupLabel>
          <SidebarGroupContent>
            <SidebarMenu>
              <SidebarMenuItem>
                <SidebarMenuButton tooltip="Servers" render={<Link href="/" />}>
                  <List />
                  <span>All servers</span>
                </SidebarMenuButton>
              </SidebarMenuItem>
              <SidebarMenuItem>
                <SidebarMenuButton tooltip="Access" render={<Link href="/access/" />}>
                  <ShieldCheck />
                  <span>Access</span>
                </SidebarMenuButton>
              </SidebarMenuItem>
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>
    </Sidebar>
  )
}

/** Every server this account can open, with the one Aerona is not in marked as such -
 *  switching here is how you do not have to go back to the list to change guild. */
function GuildSwitcher({ current }: { current: { id: string; name: string; icon: string | null; botPresent: boolean } }) {
  const { data } = useAsync(() => fetchGuilds(), [])
  const [open, setOpen] = useState(false)
  const rows = useMemo(() => data?.guilds ?? [], [data])

  return (
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger>
        <SidebarMenuButton size="lg" aria-label="Switch server">
          <Avatar className="size-8 rounded-lg">
            <AvatarImage src={iconUrl(current.id, current.icon, 64) ?? undefined} alt="" />
            <AvatarFallback className="rounded-lg text-[10px]">{guildInitials(current.name)}</AvatarFallback>
          </Avatar>
          <span className="grid flex-1 text-left leading-tight">
            <span className="truncate font-medium">{current.name}</span>
            <span className="truncate text-xs text-muted-foreground">{current.botPresent ? 'ready' : 'add the bot'}</span>
          </span>
          <ChevronDown className="size-4 opacity-60" />
        </SidebarMenuButton>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" side="bottom" className="w-64">
        <DropdownMenuLabel>Servers</DropdownMenuLabel>
        <DropdownMenuSeparator />
        {rows.length === 0 ? (
          <DropdownMenuItem disabled>Nothing else here</DropdownMenuItem>
        ) : (
          rows.map((g) => (
            <DropdownMenuItem key={g.id} render={<Link href={guildPath(g.id, '')} />}>
              <Avatar className="size-6 rounded-md">
                <AvatarImage src={iconUrl(g.id, g.icon, 32) ?? undefined} alt="" />
                <AvatarFallback className="rounded-md text-[9px]">{guildInitials(g.name)}</AvatarFallback>
              </Avatar>
              <span className="truncate">{g.name}</span>
              {g.id === current.id ? <Check className="ms-auto size-4" /> : null}
              {!g.botPresent ? <Badge variant="outline">add</Badge> : null}
            </DropdownMenuItem>
          ))
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function TopBar({ skeleton }: { skeleton?: boolean }) {
  const { id, page, data, name } = useGuild()
  const def = pageFor(page)
  const reduce = useReducedMotion()
  return (
    <header className="sticky top-0 z-10 flex h-14 items-center gap-2 border-b bg-background/80 px-4 backdrop-blur">
      <SidebarTrigger />
      <div className="flex min-w-0 items-center gap-2 text-sm">
        <span className="truncate font-medium">{name}</span>
        <span className="text-muted-foreground">/</span>
        <span className="truncate text-muted-foreground">{def?.label ?? 'Overview'}</span>
      </div>
      <div className="ms-auto flex items-center gap-2">
        {skeleton ? (
          <Skeleton className="h-6 w-24 rounded-full" />
        ) : (
          <>
            {data?.raid?.active ? <Badge variant="destructive">raid mode</Badge> : null}
            <span className="hidden text-xs text-muted-foreground sm:inline">
              {data?.live ? `bot online · ${id}` : `bot idle · ${id}`}
            </span>
          </>
        )}
        <ThemeToggle />
      </div>
    </header>
  )
}

/** Icons are decorative; the label carries the meaning, so an unknown page is not a crash. */
