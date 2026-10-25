/**
 * The prerender entry. It renders one route to a static string; `scripts/prerender.mjs`
 * stitches that into `index.html` with the page's own head tags.
 *
 * Nothing here fetches: app pages render their chrome and empty states only, because
 * a session cookie cannot exist at build time and a stale moderation table on a
 * crawler is worse than an honest skeleton.
 */
import { renderToStaticMarkup } from 'react-dom/server'

import { preload } from './App'
import { App } from './App'
import { headTags } from './lib/head'
import { matchRoute, routes } from './router'
import { site } from './site'

export const paths = routes.filter((r) => !r.dynamic).map((r) => r.path)

export type Rendered = { html: string; head: string; path: string }

/** Render one route and produce the head that belongs with it. The prerenderer writes
 *  both into `index.html`, so nothing else has to know how a title is assembled. */
export async function render(url: string, origin?: string): Promise<Rendered> {
  const match = matchRoute(url)
  if (match) await preload(match.def)
  const html = renderToStaticMarkup(<App initialPath={url} />)
  const meta = match ? match.def.meta(match.params) : { path: url, kind: 'WebPage' as const, title: 'Not found', description: '' }
  // `origin` comes from the build environment: a self-hosted tool cannot know its own
  // hostname at build time, so an unset origin leaves a placeholder canonical rather
  // than guessing one that would be wrong for most installs.
  return { html, head: headTags(origin || site.origin, meta), path: url }
}
