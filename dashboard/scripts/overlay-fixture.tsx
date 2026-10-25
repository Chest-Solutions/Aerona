/**
 * Every popup the panel can open, rendered with `react-dom/server` so a crash in a *menu* is a
 * build failure and not a blank page. `scripts/audit-overlays.mjs` builds and runs this.
 *
 * The shape is not decorative: it is what the pages actually render. The guild switcher's caption
 * sat directly under `DropdownMenuContent` with no group around it, Base UI's `Menu.GroupLabel`
 * threw reading `MenuGroupContext`, and the throw took the guild page with it — two rounds of
 * "the tabs don't work" for one line of console. A static audit can say a caption is not a group
 * label; only rendering says the rest of these parts are happy where the app puts them.
 */
import { renderToStaticMarkup } from 'react-dom/server'
import type { ReactElement } from 'react'

import { matchRoute } from '../src/router'
import { guildPages, guildPath } from '../src/lib/nav'
import { metaFor } from '../src/pages-meta'

import {
  DropdownMenu, DropdownMenuCheckboxItem, DropdownMenuContent, DropdownMenuGroup,
  DropdownMenuGroupLabel, DropdownMenuItem, DropdownMenuLabel, DropdownMenuRadioGroup,
  DropdownMenuRadioItem, DropdownMenuSeparator, DropdownMenuShortcut, DropdownMenuTrigger,
} from '../src/components/ui/dropdown-menu'
import {
  Select, SelectContent, SelectGroup, SelectGroupLabel, SelectItem, SelectLabel, SelectSeparator,
  SelectTrigger, SelectValue,
} from '../src/components/ui/select'
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '../src/components/ui/tooltip'
import { Popover, PopoverContent, PopoverTrigger } from '../src/components/ui/popover'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger } from '../src/components/ui/dialog'
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle, SheetTrigger } from '../src/components/ui/sheet'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '../src/components/ui/tabs'
import { Avatar, AvatarFallback, AvatarImage } from '../src/components/ui/avatar'
import { Switch } from '../src/components/ui/switch'
import { Progress, ProgressLabel, ProgressValue } from '../src/components/ui/progress'
import { HoverCard, HoverCardContent, HoverCardTrigger } from '../src/components/ui/hover-card'

/**
 * What "covered" means here, because the honest answer is: not everything.
 *
 * `renderToStaticMarkup` does not run effects, and Base UI mounts a popup inside an effect — its
 * `Portal` renders `null` until the store says `mounted`. So the popup *contents* of a menu, select,
 * tooltip, dialog, sheet, popover or hover card are out of reach of a server render, and the
 * `popup: true` cases below are compiled-and-imported-but-not-asserted on purpose: they still catch a
 * part used outside its root *around* the portal, and they would catch a crash in a wrapper's own
 * body, but a caption inside a closed popup is not read by this harness. Asserting otherwise would be
 * the same mistake as the bug this file exists to prevent — a check that reports a pass it did not
 * earn. The static rule in `prerender.mjs` ("a caption must not be a group-scoped primitive") is what
 * covers the popup case, and `verify-panel.sh` covers the shipped document.
 *
 * `marker` is what makes an assertion an assertion: every `popup: false` case has to produce that
 * text, or the runner fails. A harness that renders nothing and reports twelve passes is worse than
 * no harness, because it is read as evidence.
 */
/**
 * A `check` case is not markup: it calls the real functions and returns the sentence explaining why
 * they disagree, or `null` when they agree. It belongs here rather than in a unit-test file because
 * the thing worth catching is a *composition* fault — a route table that produces params the page
 * does not read, which type-checks cleanly (both sides are `Record<string, string>`) and renders a
 * page that never changes. No type can see that; calling `matchRoute` does.
 */
type Case =
  | { name: string; popup: boolean; marker?: string; render: () => ReactElement; check?: never }
  | { name: string; check: () => string | null; render?: never; popup?: never; marker?: never }

/**
 * `/guild/<id>/<page>/` is the whole app, so this is the case that matters most: the router must hand
 * `pages/Guild.tsx` the segment it switches on. Every nav row is walked, because a row that matches no
 * route, or matches one that loses its segment, is a sidebar link that silently reloads the same view.
 */
function checkGuildRouting(): string | null {
  const bad: string[] = []
  for (const page of guildPages) {
    const path = guildPath('1234', page.seg)
    const m = matchRoute(path)
    if (!m) {
      bad.push(`${path} matches no route`)
      continue
    }
    if (m.params.guild !== '1234') bad.push(`${path} lost the guild id (params.guild=${JSON.stringify(m.params.guild)})`)
    // `pages/Guild.tsx` reads `params.page` and nothing else, so this is the only shape that works.
    if ((m.params.page ?? '') !== page.seg) {
      bad.push(`${path} yields page=${JSON.stringify(m.params.page ?? '')} but the row's segment is ${JSON.stringify(page.seg)} — the view would not change`)
    }
    try {
      metaFor(`Guild:${page.seg}`)
    } catch {
      bad.push(`${page.seg || 'overview'} has no head entry, so the title cannot follow the tab`)
    }
  }
  if (matchRoute('/guild/1234/not-a-page/')) bad.push('/guild/1234/not-a-page/ is routable, so a typo renders a view instead of the 404')
  return bad.length ? bad.join('; ') : null
}


/** Each case is the markup a real page produces, at the nesting that page uses. */
const cases: Case[] = [
  { name: 'guild routing', check: checkGuildRouting },
  { name: 'guild switcher', popup: true, render: () => (
    <DropdownMenu open>
      <DropdownMenuTrigger>Switch server</DropdownMenuTrigger>
      <DropdownMenuContent align="start" side="bottom">
        <DropdownMenuLabel>Servers</DropdownMenuLabel>
        <DropdownMenuSeparator />
        <DropdownMenuItem render={<a href="/guild/1/" />}>One</DropdownMenuItem>
        <DropdownMenuItem disabled>Nothing else here</DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  )},
  { name: 'grouped menu', popup: true, render: () => (
    <DropdownMenu open>
      <DropdownMenuTrigger>Per-server action</DropdownMenuTrigger>
      <DropdownMenuContent>
        <DropdownMenuGroup>
          <DropdownMenuGroupLabel>Sticky</DropdownMenuGroupLabel>
          <DropdownMenuCheckboxItem checked>Pin announcements</DropdownMenuCheckboxItem>
        </DropdownMenuGroup>
        <DropdownMenuSeparator />
        <DropdownMenuRadioGroup value="warn">
          <DropdownMenuRadioItem value="warn">Warn <DropdownMenuShortcut>w</DropdownMenuShortcut></DropdownMenuRadioItem>
          <DropdownMenuRadioItem value="mute">Mute</DropdownMenuRadioItem>
        </DropdownMenuRadioGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  )},
  { name: 'channel picker', popup: true, render: () => (
    <Select open>
      <SelectTrigger><SelectValue placeholder="Pick a channel…" /></SelectTrigger>
      <SelectContent>
        <SelectLabel>Text</SelectLabel>
        <SelectItem value="1">#general</SelectItem>
        <SelectSeparator />
        <SelectGroup>
          <SelectGroupLabel>Voice</SelectGroupLabel>
          <SelectItem value="2">Lounge</SelectItem>
        </SelectGroup>
      </SelectContent>
    </Select>
  )},
  { name: 'sidebar tooltip', popup: true, render: () => (
    <TooltipProvider delay={0}>
      <Tooltip open>
        <TooltipTrigger>Moderation</TooltipTrigger>
        <TooltipContent side="right">Moderation</TooltipContent>
      </Tooltip>
    </TooltipProvider>
  )},
  { name: 'tooltip with no provider', popup: true, render: () => (
    <Tooltip open>
      <TooltipTrigger>Moderation</TooltipTrigger>
      <TooltipContent side="right">Moderation</TooltipContent>
    </Tooltip>
  )},
  { name: 'dialog', popup: true, render: () => (
    <Dialog open>
      <DialogTrigger>Delete</DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Remove the strike?</DialogTitle>
          <DialogDescription>The member keeps the message.</DialogDescription>
        </DialogHeader>
        <DialogFooter>Confirm</DialogFooter>
      </DialogContent>
    </Dialog>
  )},
  { name: 'sheet', popup: true, render: () => (
    <Sheet open>
      <SheetTrigger>Filters</SheetTrigger>
      <SheetContent>
        <SheetHeader>
          <SheetTitle>Text filter</SheetTitle>
          <SheetDescription>Rules this server applies to every message.</SheetDescription>
        </SheetHeader>
      </SheetContent>
    </Sheet>
  )},
  { name: 'popover', popup: true, render: () => (
    <Popover open>
      <PopoverTrigger>Set</PopoverTrigger>
      <PopoverContent>Threshold 0.7</PopoverContent>
    </Popover>
  )},
  { name: 'hover card', popup: true, render: () => (
    <HoverCard open>
      <HoverCardTrigger>@mod</HoverCardTrigger>
      <HoverCardContent>Member since 2019</HoverCardContent>
    </HoverCard>
  )},
  { name: 'tabs', popup: false, marker: 'Nothing pending', render: () => (
    <Tabs value="a">
      <TabsList>
        <TabsTrigger value="a">Warns</TabsTrigger>
        <TabsTrigger value="b">Audit</TabsTrigger>
      </TabsList>
      <TabsContent value="a">Nothing pending</TabsContent>
    </Tabs>
  )},
  // `Progress` renders its own track and indicator, so the parts stay inside their root when a page
  // uses it; that nesting is the whole reason this case is here rather than three lines of markup.
  { name: 'progress', popup: false, marker: 'Strike 1 of 3', render: () => (
    <Progress value={60}>
      <ProgressLabel>Strike 1 of 3</ProgressLabel>
      <ProgressValue />
    </Progress>
  )},
  { name: 'avatar and switch', popup: false, marker: 'AB', render: () => (
    <div>
      <Avatar><AvatarImage src="/a.png" alt="" /><AvatarFallback>AB</AvatarFallback></Avatar>
      <Switch checked />
    </div>
  )},
]

export function run(): { failures: string[]; asserted: number; unasserted: string[] } {
  const failures: string[] = []
  const unasserted: string[] = []
  let asserted = 0
  for (const c of cases) {
    if (c.check) {
      const why = c.check()
      if (why) failures.push(`${c.name}: ${why}`)
      else asserted += 1
      continue
    }
    let html = ''
    try {
      html = renderToStaticMarkup(c.render())
    } catch (error) {
      failures.push(`${c.name}: ${(error as Error).message.split('\n')[0]}`)
      continue
    }
    if (c.marker) {
      if (!html.includes(c.marker)) {
        failures.push(`${c.name}: rendered without "${c.marker}" — the assertion is not being reached, which is not a pass`)
      } else {
        asserted += 1
      }
      continue
    }
    if (c.popup && html.length > 0) unasserted.push(c.name)
    else if (!c.popup) failures.push(`${c.name}: rendered nothing at all, so nothing was checked`)
  }
  return { failures, asserted, unasserted }
}
