/**
 * The shell every page renders inside: theme, router, session, one Suspense boundary.
 *
 * There is one shape now, because there is one kind of page now: the install *is* the
 * dashboard, and it does not show itself to a browser it cannot identify. `<main id="main">`
 * is load-bearing twice - `main.tsx` treats it as the mark of prerendered markup, and the
 * router moves focus to it after a navigation so a screen reader is not left on a link that
 * no longer exists.
 */
import { Suspense, useEffect, use } from 'react'
import type { ComponentType, ReactNode } from 'react'

import { Skeleton } from './components/ui/skeleton'
import { Toaster } from './components/ui/sonner'
import { ThemeProvider } from './components/theme-toggle'
import { SessionProvider, useSession } from './lib/hooks'
import { RouterProvider, routes, useRouter, type RouteDef, type RouteParams } from './router'

// Pages are lazy, but the prerenderer resolves the module before it renders, so a
// static pass never suspends - one code path, two modes.
const resolved = new Map<string, ComponentType<{ params: RouteParams }>>()
const pending = new Map<string, Promise<ComponentType<{ params: RouteParams }>>>()

function pageComponent(def: RouteDef): ComponentType<{ params: RouteParams }> {
  const hit = resolved.get(def.path)
  if (hit) return hit
  let promise = pending.get(def.path)
  if (!promise) {
    promise = def.load().then((m) => {
      resolved.set(def.path, m.default)
      pending.delete(def.path)
      return m.default
    })
    pending.set(def.path, promise)
  }
  return use(promise)
}

/** Used by `entry-server.tsx` before it renders a page. */
export async function preload(def: RouteDef): Promise<void> {
  if (resolved.has(def.path)) return
  const m = await def.load()
  resolved.set(def.path, m.default)
}

/**
 * The door, owned by the shell rather than by any page.
 *
 * A route marked `app` in `router.tsx` is a signed-in route, and a browser with no session
 * gets one page to look at: the login screen, asked in the imperative, carrying where it was
 * going. That is the whole behaviour - no server list with a "could not load" card under it,
 * and no API error body where a page should be, because a 401 from a data fetch is not a fact
 * about the install. It is a fact about the visitor, and the visitor is about to be sent
 * somewhere else.
 *
 * The check is `ready && !me`, not `!me`: while `/me` is in flight there is nothing to decide,
 * and hiding the view then would blank a signed-in page for a round trip and strip the
 * prerendered document before hydration. Once it settles on "no session", the view is not
 * rendered at all - so a fetch that 401s on the same tick cannot flash its error card first.
 */
function AuthGate({ children }: { children: ReactNode }) {
  const { me, ready } = useSession()
  const { match, path, search, navigate } = useRouter()
  const gated = Boolean(match?.def.app)
  const signedOut = ready && !me
  const where = search.toString()
  const next = path === '/' ? '/' : `${path}${where ? `?${where}` : ''}`

  useEffect(() => {
    if (gated && signedOut) {
      const target = next === '/' ? '/login/' : `/login/?next=${encodeURIComponent(next)}`
      navigate(target, { replace: true })
    }
  }, [gated, signedOut, next, navigate])

  // `ready` is false for the first frames and during prerender, and the decision is not made
  // yet then: a signed-in visitor would otherwise see the page they are allowed to read blank
  // out while `/me` is in flight, and the prerendered document would lose its markup before
  // hydration even began. Only a *settled* "no session" hides the view - which is also what
  // keeps a data fetch's 401 from being painted as an error card for one frame.
  if (gated && ready && !me) return null
  return <>{children}</>
}

function Shell() {
  const { match, params } = useRouter()
  // An unmatched path renders the 404 page rather than a fourth layout: the API already
  // answers an unknown URL with the prerendered `/404/` document, and this is the same
  // verdict when the client's own history changes underneath us.
  const def = match?.def ?? routes.find((r) => r.path === '/404/') ?? routes[0]!
  const Page = pageComponent(def)
  return (
    <>
      <a
        href="#main"
        className="sr-only focus:not-sr-only focus:fixed focus:top-2 focus:left-2 focus:z-50 focus:rounded-lg focus:bg-background focus:px-3 focus:py-2 focus:text-sm focus:shadow"
      >
        Skip to content
      </a>
      <main id="main" tabIndex={-1} className="min-h-svh outline-none">
        <Suspense fallback={<PageSkeleton />}>
          <Page params={match?.params ?? params} />
        </Suspense>
      </main>
      {/* Sonner, not a bespoke toast: `role="status"` in a live region, so a saved setting
          is announced, and every page calls `toast` rather than threading a context. */}
      <Toaster position="bottom-right" closeButton />
    </>
  )
}

/** Before the bundle has chosen a page, and before a view has data: the frame the panel
 *  actually uses, so the first paint and the settled one have the same gutter and `#main`. */
function PageSkeleton() {
  return (
    <div className="mx-auto w-full max-w-6xl px-5 py-6">
      <Skeleton className="h-9 w-48 rounded-lg" />
      <div className="mt-5 grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
        {[0, 1, 2, 3].map((i) => (
          <Skeleton key={i} className="h-28 rounded-xl" />
        ))}
      </div>
    </div>
  )
}

export function App({ initialPath }: { initialPath?: string }) {
  return (
    <ThemeProvider>
      <RouterProvider initialPath={initialPath}>
        <SessionProvider>
          <AuthGate>
            <Shell />
          </AuthGate>
        </SessionProvider>
      </RouterProvider>
    </ThemeProvider>
  )
}
