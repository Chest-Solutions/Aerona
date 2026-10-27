/**
 * Page metadata in one table, next to the route table, because the head must be correct in
 * the server-rendered HTML - an async `meta()` export per page would arrive a chunk later
 * than the paint it describes.
 *
 * Every route here is the panel, so every route is `noindex`: a moderation dashboard behind
 * a Discord login has nothing to publish, and a search result pointing at an empty `/tickets/`
 * of somebody else's server would be a leak of a different kind. The head builder reads this
 * table to decide robots, canonical and JSON-LD, and the prerender audit asserts that a
 * private route really did get its `noindex` - so this is the only place to change it.
 */
import type { PageMeta } from './lib/meta'
import { guildPages, guildPath } from './lib/nav'

const app = (label: string, path: string, description: string): PageMeta => ({
  path,
  kind: 'WebPage',
  noindex: true,
  title: `${label} - Aerona dashboard`,
  description,
  crumbs: path === '/' ? undefined : [{ label: 'Dashboard', href: '/' }],
})

/** The route table looks its head up by key. A key with no entry here is a build error
 *  rather than a page that renders with someone else's title, so it throws instead of
 *  defaulting - and the prerender pass fails loudly on it, which is the only time a
 *  mislabelled `<title>` can be caught before a deploy. */
export function metaFor(key: string): PageMeta {
  const m = pagesMeta[key]
  if (!m) throw new Error(`pages-meta has no entry for "${key}"`)
  return m
}

export const pagesMeta: Record<string, PageMeta> = {
  // The root is the server list, so its title drops the suffix: a document that *is* the
  // product should be called the product. Every other view keeps `Thing - Aerona dashboard`,
  // because a browser with ten tabs open reads the tab, not the URL.
  Guilds: { ...app('Servers', '/', 'Every server this Discord account administers, and the state of Aerona in each one.'), title: 'Aerona dashboard' },
  Access: app('Access', '/access/', 'Sessions and API keys for this install, and what each of them is allowed to touch.'),
  // One entry per guild page, generated from the same table the sidebar renders. A page that
  // is routable but has no head entry throws at build, which is the only moment a
  // mislabelled `<title>` can be caught before a deploy.
  ...Object.fromEntries(
    guildPages.map((g) => [
      `Guild:${g.seg}`,
      app(g.label, guildPath(':guild', g.seg), `${g.hint} Configured per server, by the account that administers it.`),
    ]),
  ),
  Login: {
    path: '/login/',
    kind: 'WebPage',
    noindex: true,
    title: 'Sign in - Aerona',
    description: 'Sign in with the Discord account that administers your servers. The Aerona dashboard shows nothing until you do.',
  },
  NotFound: {
    path: '/404/',
    kind: 'WebPage',
    noindex: true,
    title: 'Not found - Aerona',
    description: 'That page is not part of this installation.',
  },
}
