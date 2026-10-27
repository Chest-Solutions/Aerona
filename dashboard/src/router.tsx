/**
 * A deliberately small router: a table of routes, the history API, and one context.
 *
 * It exists in this shape because the prerenderer has to walk it (`routes` is plain
 * data, not JSX) and because every page must supply `meta()` - a route without SEO
 * data is a build error here, not an oversight.
 */
import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react'
import type { ReactNode } from 'react'
import type { AnchorHTMLAttributes } from 'react'
import type { ComponentType } from 'react'

import { applyHead } from './lib/head'
import type { PageMeta } from './lib/meta'
import { guildPath, pageFor } from './lib/nav'
import { metaFor, pagesMeta } from './pages-meta'
import { withSlash } from './lib/utils'

export type RouteParams = Record<string, string>

export type RouteDef = {
  path: string
  /** App routes are auth-gated, never indexed, and never prerendered with data. */
  app?: boolean
  /** A pattern with a segment the build cannot know (`:guild`). The prerenderer skips
   *  these: writing `/guild/:guild/index.html` would publish a folder with a placeholder
   *  in its name, and the client router is what turns the pattern into a page. */
  dynamic?: boolean
  meta: (params: RouteParams) => PageMeta
  /**
   * A pattern can match a path the product does not have: `/guild/<id>/:page/` accepts
   * `/guild/1234/notapage/`, and rendering that as a page is how a typo becomes an empty view
   * with a 200. Returning false here means "this route does not match after all", so the path
   * falls through to the 404 document the API already serves, and `isKnownPath` - the link
   * audit's oracle - agrees with the router rather than being a second opinion.
   */
  check?: (params: RouteParams) => boolean
  load: () => Promise<{ default: ComponentType<RouteProps> }>
}

export type RouteProps = { params: RouteParams }

const R = (key: string, load: () => Promise<{ default: ComponentType<RouteProps> }>, app = false): RouteDef => ({
  path: pagesMeta[key]!.path,
  app,
  meta: () => metaFor(key),
  load,
})

export const routes: RouteDef[] = [
  // The whole product: the server list at `/`, one page per feature inside a server, and the
  // two screens a browser needs before and after them. There are no marketing routes to list,
  // so `knownPaths()` below is short and the prerender audit's "internal link goes nowhere"
  // rule has nothing left to forgive.
  R('Login', () => import('./pages/Login')),
  R('Guilds', () => import('./pages/Guilds'), true),
  // `/guild/123/tickets/`. Two patterns, not one per nav row, and the difference is the whole
  // page: a literal segment (`/guild/:guild/tickets/`) matches the URL but puts nothing in
  // `params`, while `pages/Guild.tsx` switches on `params.page`. That pair of facts is how every
  // tab link came to rewrite the URL and re-render the overview - no error, no request, because
  // the segment was never read, only matched. A *named* segment is what carries it.
  //
  // What the nav table still owns is which pages exist at all: `check` asks `pageFor`, so a row
  // added to `nav.ts` is routable without touching this file, and a path that is not a row is a
  // 404 rather than a blank pane. The head follows the same table, per page.
  {
    path: guildPath(':guild', ''),
    app: true,
    dynamic: true,
    meta: () => metaFor('Guild:'),
    load: () => import('./pages/Guild'),
  },
  {
    path: guildPath(':guild', ':page'),
    app: true,
    dynamic: true,
    check: (p) => Boolean(pageFor(p.page ?? '')),
    meta: (p) => metaFor(`Guild:${p.page ?? ''}`),
    load: () => import('./pages/Guild'),
  },
  R('Access', () => import('./pages/Access'), true),
  R('NotFound', () => import('./pages/NotFound')),
]

/** One module, several named page components: `Legal.tsx` exports `Privacy`/`Terms`,
 *  `AppShell.tsx` exports every app view. */
function pickDefault(name: string) {
  return (mod: Record<string, unknown>) => {
    const comp = (mod[name] ?? mod.default) as ComponentType<RouteProps>
    return { default: comp }
  }
}

/** Every path the client can route to, with the guild pattern left as a pattern. The
 *  prerender audit uses it to decide whether an `href` in a rendered page goes nowhere. */
export function isKnownPath(pathname: string): boolean {
  return matchRoute(pathname) !== null
}

// ------------------------------------------------------------------ matching

const compiled = routes.map((r) => ({
  def: r,
  parts: withSlash(r.path).split('/').filter(Boolean),
}))

export type Match = { def: RouteDef; params: RouteParams }

export function matchRoute(pathname: string): Match | null {
  const path = withSlash(pathname)
  const parts = path.split('/').filter(Boolean)
  for (const { def, parts: want } of compiled) {
    if (want.length !== parts.length) continue
    const params: RouteParams = {}
    let ok = true
    for (let i = 0; i < want.length; i++) {
      const w = want[i]!
      const p = parts[i]!
      if (w.startsWith(':')) params[w.slice(1)] = decodeURIComponent(p)
      else if (w !== p) {
        ok = false
        break
      }
    }
    if (ok && def.check) ok = def.check(params)
    if (ok) return { def, params }
  }
  return null
}

/** Every internal link, for the prerender audit: it must resolve to a route. */
export function knownPaths(): string[] {
  return routes.map((r) => withSlash(r.path))
}

// ------------------------------------------------------------------ context

type Ctx = {
  path: string
  params: RouteParams
  match: Match | null
  navigate: (to: string, opts?: { replace?: boolean }) => void
  /** The query string of the current location. The login page needs `?error=` and nothing
   *  else does, which is why it is read here instead of every page opening a `URL`. */
  search: URLSearchParams
}
const RouterContext = createContext<Ctx>({ path: '/', params: {}, match: null, navigate: () => {}, search: new URLSearchParams() })
export const useRouter = (): Ctx => useContext(RouterContext)

export function RouterProvider({ children, initialPath }: { children: ReactNode; initialPath?: string }) {
  // `initialPath` is what the prerenderer passes; a server render has no location.
  const [path, setPath] = useState(() => initialPath ?? (typeof window === 'undefined' ? '/' : window.location.pathname))

  useEffect(() => {
    if (typeof window === 'undefined') return
    const onPop = () => setPath(window.location.pathname)
    window.addEventListener('popstate', onPop)
    return () => window.removeEventListener('popstate', onPop)
  }, [])

  const navigate = useCallback((to: string, opts?: { replace?: boolean }) => {
    const url = new URL(to, window.location.origin)
    const same = url.pathname === window.location.pathname && !url.search
    if (url.origin !== window.location.origin || url.pathname.startsWith('/api/')) {
      window.location.href = to
      return
    }
    if (same && url.search === window.location.search) {
      window.scrollTo({ top: 0, behavior: 'auto' })
      return
    }
    if (opts?.replace) window.history.replaceState({}, '', url.pathname + url.search)
    else window.history.pushState({}, '', url.pathname + url.search)
    setSearch(new URLSearchParams(url.search))
    setPath(url.pathname)
  }, [])

  const [search, setSearch] = useState(() => (typeof window === 'undefined' ? new URLSearchParams() : new URLSearchParams(window.location.search)))
  useEffect(() => {
    if (typeof window === 'undefined') return
    const sync = () => setSearch(new URLSearchParams(window.location.search))
    window.addEventListener('popstate', sync)
    return () => window.removeEventListener('popstate', sync)
  }, [])

  const value = useMemo<Ctx>(() => {
    const m = matchRoute(path)
    return { path, params: m?.params ?? {}, match: m, navigate, search }
    // `navigate` rewrites the query it pushes, so the params are re-read there rather than
    // left to a `popstate` that a `pushState` never fires.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, navigate, search])

  // On navigation: scroll to the top, move focus to <main> (screen readers must not be
  // left on a link that no longer exists), and update the document head.
  useEffect(() => {
    if (typeof document === 'undefined') return
    const m = matchRoute(path)
    applyHead(m ? m.def.meta(m.params) : { path, kind: 'WebPage', title: 'Not found - Aerona', description: 'This page does not exist.' })
    if (!m) return
    document.getElementById('main')?.focus({ preventScroll: true })
    window.scrollTo({ top: 0, behavior: 'auto' })
  }, [path])

  return <RouterContext.Provider value={value}>{children}</RouterContext.Provider>
}

export function Link({
  href,
  to,
  children,
  className,
  ...rest
}: { href?: string; to?: string; children?: ReactNode; className?: string } & Omit<AnchorHTMLAttributes<HTMLAnchorElement>, 'href'>) {
  // Both spellings work: `<Link to>` reads better in an app, `<Link href>` in prose.
  const dest = href ?? to ?? '#'
  const { navigate } = useRouter()
  const external = /^https?:/.test(dest) || dest.startsWith('mailto:')
  if (external) {
    return (
      <a href={dest} className={className} target="_blank" rel="noreferrer" {...rest}>
        {children}
      </a>
    )
  }
  return (
    <a
      href={dest}
      className={className}
      {...rest}
      onClick={(e) => {
        if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return
        e.preventDefault()
        navigate(dest)
      }}
    >
      {children}
    </a>
  )
}
