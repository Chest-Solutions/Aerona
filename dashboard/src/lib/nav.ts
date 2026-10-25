/**
 * The guild sidebar, in one table.
 *
 * Each row carries its icon, so the rail, the page head and the audit all read one table.
 *
 * A page is listed once, here. The router builds its route patterns from it, the sidebar
 * renders its groups from it, and the settings form for a page looks up which config
 * document it writes — so adding a feature means adding a row, not editing four files.
 */
import type { LucideIcon } from 'lucide-react'

export type GuildPage = {
  /** URL segment after `/guild/<id>/`. */
  seg: string
  label: string
  /** One line, under the heading. Never a paragraph. */
  hint: string
  group: 'Server' | 'Safety' | 'Community' | 'Setup'
  /** Which `settings/<feature>` documents this page edits. */
  features: string[]
  /**
   * The page's own glyph, in the rail and in the header. Uniqueness is audited at build time
   * rather than left to taste: fifteen rows sharing three icons (which is what this table used to
   * have — a shield on nine of them) means the rail cannot be scanned, only read, and a symbol
   * that repeats in a nav list repeats in the operator's memory of where things live.
   */
  icon: LucideIcon
}

import { Bot, Command, Filter, Gavel, History, LayoutDashboard, Link2Off, Medal, Pin, ScrollText, Settings2, Siren, Ticket, UserPlus, Waves } from 'lucide-react'

export const guildPages: GuildPage[] = [
  { seg: '', label: 'Overview', hint: 'What the bot sees right now.', group: 'Server', features: [], icon: LayoutDashboard},

  { seg: 'moderation', label: 'Moderation', hint: 'Warns, strikes, escalation.', group: 'Safety', features: ['moderation'], icon: Gavel},
  { seg: 'filter', label: 'Text filter', hint: 'Model + custom rules per message.', group: 'Safety', features: ['ai_filter'], icon: Filter},
  { seg: 'links', label: 'Links & invites', hint: 'Block invites and links. No API needed.', group: 'Safety', features: ['link_guard'], icon: Link2Off},
  { seg: 'antispam', label: 'Anti-spam', hint: 'Floods, pings, caps, emoji.', group: 'Safety', features: ['anti_spam'], icon: Waves},
  { seg: 'raid', label: 'Anti-raid', hint: 'Join bursts and a one-click lockdown.', group: 'Safety', features: ['anti_raid'], icon: Siren},
  { seg: 'tickets', label: 'Tickets', hint: 'Private threads with your staff.', group: 'Community', features: ['tickets'], icon: Ticket},
  { seg: 'stickies', label: 'Stickies', hint: 'Pinned notes that expire themselves.', group: 'Community', features: ['sticky'], icon: Pin},
  { seg: 'welcome', label: 'Welcome & roles', hint: 'Join message, autoroles, goodbye.', group: 'Community', features: ['welcome', 'autorole'], icon: UserPlus},
  { seg: 'levels', label: 'Levels', hint: 'XP, ranks, level-up pings.', group: 'Community', features: ['general'], icon: Medal},
  { seg: 'logs', label: 'Logging', hint: 'Edits, deletes, name changes.', group: 'Community', features: ['logging'], icon: ScrollText},

  { seg: 'commands', label: 'Commands', hint: 'What /aerona offers in this server.', group: 'Setup', features: ['commands'], icon: Command},
  { seg: 'profile', label: 'Bot profile', hint: 'Its name, avatar and bio here.', group: 'Setup', features: ['identity'], icon: Bot},
  { seg: 'settings', label: 'Server settings', hint: 'Prefix, staff roles, ignores.', group: 'Setup', features: ['general'], icon: Settings2},
  { seg: 'audit', label: 'Audit', hint: 'Every change, hash-chained.', group: 'Setup', features: [], icon: History},
]

export const guildGroups = ['Server', 'Safety', 'Community', 'Setup'] as const

/** `/guild/123/` and `/guild/123/moderation/` — trailing slash, like every other route. */
export function guildPath(guild: string, seg: string): string {
  return `/guild/${guild}/${seg ? `${seg}/` : ''}`
}

export function pageFor(seg: string): GuildPage | undefined {
  return guildPages.find((p) => p.seg === seg)
}
