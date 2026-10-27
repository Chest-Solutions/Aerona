/**
 * Everything brand-shaped, in one file, so a rebrand is a single diff.
 *
 * There is no marketing site any more: an Aerona install *is* the dashboard, and these
 * values are what the document head, the manifest and the JSON-LD node are built from. The
 * `origin` is only used to assemble absolute URLs at prerender time - every link a browser
 * follows is relative, which is what lets the API serve the panel from whatever host the
 * operator typed without a rebuild.
 */
export const site: {
  name: string
  legalName: string
  tagline: string
  title: string
  description: string
  ogTitle: string
  ogDescription: string
  origin: string
  /** BCP-47. Drives `og:locale` and `inLanguage`. */
  locale: string
  email: string
  github: string
  appNav: { href: string; label: string }[]
} = {
  name: 'Aerona',
  legalName: 'Aerona',
  tagline: 'One bot, one dashboard, one SQLite file.',
  title: 'Aerona dashboard',
  description:
    'The dashboard for a self-hosted Aerona install: moderation, tickets, sticky messages, anti-raid and the text filter, one configuration per server.',
  ogTitle: 'Aerona dashboard',
  ogDescription: 'Sign in with Discord and configure the servers you administer.',
  origin: (import.meta as unknown as { env?: Record<string, string> }).env?.AERONA_SITE_ORIGIN ?? 'https://aerona.local',
  locale: 'en-US',
  // The JSON-LD organization node wants a contact; a self-hosted install can point
  // this at whoever runs it, and nothing else reads it.
  email: 'mailto:ops@aerona.local',
  github: 'https://github.com/aerona/aerona',
  // The sidebar, in the order the operator meets it. `Servers` is second because on a fresh
  // install it is the first thing that works: it lists what this account administers and
  // carries the invite for the servers the bot is not in yet.
  appNav: [
    { href: '/', label: 'Overview' },
    { href: '/servers/', label: 'Servers' },
    { href: '/moderation/', label: 'Moderation' },
    { href: '/tickets/', label: 'Tickets' },
    { href: '/stickies/', label: 'Stickies' },
    { href: '/raid/', label: 'Anti-raid' },
    { href: '/filter/', label: 'Text filter' },
    { href: '/audit/', label: 'Audit' },
    { href: '/settings/', label: 'Settings' },
    { href: '/access/', label: 'Access' },
  ],
}

export const siteOrigin: string = site.origin
