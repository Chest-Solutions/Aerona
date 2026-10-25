/**
 * One guild's context: what the bot knows, what the worker last reported, and the one way
 * any page is allowed to change configuration.
 *
 * Every settings page goes through `patch`, which means every save is a `PATCH` to
 * `/settings/<feature>` — the endpoint that validates, merges, audits and records history.
 * No page gets to keep its own idea of how a setting is stored.
 */
import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react'
import type { ReactNode } from 'react'
import { toast } from 'sonner'

import {
  ApiError,
  guildOverview,
  guildReport,
  guilds as fetchGuilds,
  patchSettings,
  type Feature,
  type GuildOverview,
  type GuildReport,
  type GuildSummary,
} from './api'

type Ctx = {
  id: string
  name: string
  page: string
  data: GuildOverview | null
  report: GuildReport | null
  guilds: GuildSummary[]
  loading: boolean
  error: ApiError | null
  reload: () => void
  /** Whether Aerona is in this server. False is a page of its own, not a disabled form. */
  present: boolean
  inviteUrl: string | null
  settings: (f: Feature) => Record<string, unknown>
  patch: (f: Feature, doc: Record<string, unknown>) => Promise<boolean>
}

const GuildContext = createContext<Ctx | null>(null)

export function GuildProvider({
  id,
  page,
  children,
}: {
  id: string
  page: string
  children: ReactNode
}) {
  const [data, setData] = useState<GuildOverview | null>(null)
  const [report, setReport] = useState<GuildReport | null>(null)
  const [all, setAll] = useState<GuildSummary[]>([])
  const [error, setError] = useState<ApiError | null>(null)
  const [loading, setLoading] = useState(true)
  const [nonce, setNonce] = useState(0)

  const reload = useCallback(() => setNonce((n) => n + 1), [])

  useEffect(() => {
    const ac = new AbortController()
    let live = true
    setLoading(true)
    // Three requests, one render. `guilds` is what names the server in the rail and tells
    // us whether the bot is in it; the overview is everything the tiles read.
    Promise.all([guildOverview(id), guildReport(id, ac.signal), fetchGuilds()])
      .then(([o, r, g]) => {
        if (!live) return
        setData(o)
        setReport(r)
        setAll(g.guilds ?? [])
        setError(null)
      })
      .catch((e: unknown) => {
        if (!live) return
        setError(e instanceof ApiError ? e : new ApiError(0, null, 'could not reach the API'))
      })
      .finally(() => {
        if (live) setLoading(false)
      })
    return () => {
      live = false
      ac.abort()
    }
  }, [id, nonce])

  const row = useMemo(() => all.find((g) => g.id === id) ?? null, [all, id])

  const settings = useCallback(
    (f: Feature) => (data?.settings?.[f] as Record<string, unknown>) ?? {},
    [data],
  )

  const patch = useCallback(
    async (f: Feature, doc: Record<string, unknown>) => {
      try {
        const res = await patchSettings(id, f, doc)
        // Adopt the stored document, not the patch: the server fills defaults, clamps
        // ranges and is the only thing that knows what it normalised.
        setData((cur) =>
          cur ? { ...cur, settings: { ...cur.settings, [f]: res.settings } } : cur,
        )
        if (!res.changed) toast.info('Saved — nothing changed')
        else if (res.queued) toast.success(`Saved · queued for Discord as #${res.queued}`)
        else toast.success('Saved')
        // `normalize` drops keys it does not recognise. A field the server threw away is a
        // change the page will silently lose on reload, so it is said out loud.
        if (res.ignored?.length) toast.error(`Not saved: ${res.ignored.join(', ')}`)
        return true
      } catch (e) {
        toast.error(e instanceof ApiError ? e.message : 'Could not save')
        return false
      }
    },
    [id],
  )

  const value = useMemo<Ctx>(
    () => ({
      id,
      name: row?.name ?? `Server ${id}`,
      page,
      data,
      report,
      guilds: all,
      loading,
      error,
      reload,
      present: row ? row.botPresent : data !== null,
      inviteUrl: row?.inviteUrl ?? null,
      settings,
      patch,
    }),
    [id, page, data, report, all, row, loading, error, reload, settings, patch],
  )

  return <GuildContext.Provider value={value}>{children}</GuildContext.Provider>
}

export function useGuild(): Ctx {
  const ctx = useContext(GuildContext)
  if (!ctx) throw new Error('useGuild outside a guild page')
  return ctx
}
