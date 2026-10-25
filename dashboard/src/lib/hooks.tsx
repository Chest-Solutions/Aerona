import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import type React from 'react'

import { toast } from 'sonner'

import { ApiError, fetchMe, logout as signOutApi, setCsrf, type Me } from './api'

/**
 * One hook does all data fetching in the app: it aborts on unmount, keeps the
 * previous value while refetching (so a filter change does not blank the table),
 * and turns `ApiError` into something a component can render.
 */
export type Async<T> = { data: T | null; error: ApiError | null; loading: boolean; reload: () => void }

export function useAsync<T>(fn: (signal: AbortSignal) => Promise<T>, deps: unknown[]): Async<T> {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<ApiError | null>(null)
  const [loading, setLoading] = useState(true)
  const [nonce, setNonce] = useState(0)
  const [fnRef] = useState(() => ({ current: fn }))
  fnRef.current = fn

  useEffect(() => {
    const ac = new AbortController()
    let live = true
    setLoading(true)
    fnRef
      .current(ac.signal)
      .then((value) => {
        if (!live) return
        setData(value)
        setError(null)
      })
      .catch((e: unknown) => {
        if (!live || (e as Error)?.name === 'AbortError') return
        setError(e instanceof ApiError ? e : new ApiError(0, null, String((e as Error)?.message ?? e)))
      })
      .finally(() => {
        if (live) setLoading(false)
      })
    return () => {
      live = false
      ac.abort()
    }
    // The callback is read through a ref so that a new closure per render does not
    // re-run the effect; `deps` is what the caller says actually matters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, nonce])

  return { data, error, loading, reload: useCallback(() => setNonce((n) => n + 1), []) }
}

// ---------------------------------------------------------------- session

type SessionCtx = { me: Me | null; ready: boolean; refresh: () => Promise<void>; signOut: () => Promise<void> }
const SessionContext = createContext<SessionCtx>({ me: null, ready: false, refresh: async () => {}, signOut: async () => {} })

export function SessionProvider({ children }: { children: ReactNode }) {
  const [me, setMe] = useState<Me | null>(null)
  const [ready, setReady] = useState(false)
  const hadSession = useRef<Me | null>(null)
  const refresh = useCallback(async () => {
    const was = hadSession.current
    try {
      const next = await fetchMe()
      setMe(next)
      hadSession.current = next
      if (!next && was) {
        // The panel was open and the session went away under it — revoked, expired, or the
        // key changed. Say so once, in the sentence a person acts on, then let the gate
        // bounce the view to the sign-in page.
        setCsrf(null)
        toast.error('Your session ended. Sign in again.')
      }
    } catch {
      setMe(null)
      hadSession.current = null
    } finally {
      setReady(true)
    }
  }, [])
  const signOut = useCallback(async () => {
    // A failed revoke is still a sign-out from this browser's point of view: the cookie
    // is cleared either way and the server refuses the token on the next call.
    await signOutApi().catch(() => undefined)
    setMe(null)
  }, [])
  useEffect(() => {
    void refresh()
  }, [refresh])
  // A lost session ends the visit, it does not degrade it. Every request reports its 401 (see
  // `request` in api.ts), and this re-reads `/me` once — guarded on having had a session, or a
  // signed-out browser's ordinary `/me` 401 would refresh itself forever.
  useEffect(() => {
    const onUnauthorized = () => {
      if (!hadSession.current) return
      void refresh()
    }
    window.addEventListener('aerona:unauthorized', onUnauthorized)
    return () => window.removeEventListener('aerona:unauthorized', onUnauthorized)
  }, [refresh])
  const value = useMemo(() => ({ me, ready, refresh, signOut }), [me, ready, refresh, signOut])
  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>
}
export function useSession(): SessionCtx {
  return useContext(SessionContext)
}

// ------------------------------------------------------------------ misc

/** The API returns `{...}` envelopes whose inner list key varies; arrays are always
 *  read through this so a missing key renders an empty state instead of a crash. */
export function asArray<T>(v: unknown): T[] {
  return Array.isArray(v) ? (v as T[]) : []
}

export function useDebounced<T>(value: T, ms = 300): T {
  const [out, setOut] = useState(value)
  useEffect(() => {
    const id = setTimeout(() => setOut(value), ms)
    return () => clearTimeout(id)
  }, [value, ms])
  return out
}

/** `true` when the OS asks for reduced motion; every animated component reads this. */
export function useReducedMotion(): boolean {
  const [reduced, setReduced] = useState(false)
  useEffect(() => {
    if (typeof window === 'undefined' || !window.matchMedia) return
    const mq = window.matchMedia('(prefers-reduced-motion: reduce)')
    const apply = () => setReduced(mq.matches)
    apply()
    mq.addEventListener('change', apply)
    return () => mq.removeEventListener('change', apply)
  }, [])
  return reduced
}
