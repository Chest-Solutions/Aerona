/**
 * The only place the dashboard talks to the network.
 *
 * Three rules, learned from the API's own gate:
 *  1. `credentials: 'include'` — the session lives in an HttpOnly cookie, so it is
 *     never readable here and never written to storage.
 *  2. Every mutating request carries `x-aerona-csrf`. The value comes from
 *     `GET /api/v1/me`, i.e. from the server, in memory only.
 *  3. One in-flight `AbortController` per hook, aborted on navigation, so a slow
 *     response cannot land on a page the moderator already left.
 */
export const API = '/api/v1'
export const CSRF_HEADER = 'x-aerona-csrf'

export type ApiEnvelope<T> = {
  data: T
  meta?: { apiVersion?: string; at?: number }
}
export type ApiErrorBody = { error: { code: string; message: string; details?: unknown } }

export class ApiError extends Error {
  readonly status: number
  readonly code: string
  readonly details: unknown
  constructor(status: number, body: ApiErrorBody | null, fallback: string) {
    // A 401 is a fact about the visitor, not about the request, and "unauthorized" reads like a
    // bug in the product. The gate sends such a browser to the sign-in page; if any of this text
    // survives into a toast or a card, it has to read as the instruction it actually is.
    super(status === 401 ? 'Not signed in — sign in again.' : (body?.error?.message ?? fallback))
    this.name = 'ApiError'
    this.status = status
    this.code = body?.error?.code ?? (status === 0 ? 'network' : `http_${status}`)
    this.details = body?.error?.details
  }
  /** 401 means "you are not signed in", which is a route change, not a toast. */
  get unauthorized(): boolean {
    return this.status === 401
  }
}

let csrfToken: string | null = null
export function setCsrf(token: string | null): void {
  csrfToken = token
}
export function hasCsrf(): boolean {
  return csrfToken !== null
}

type FetchInit = Omit<RequestInit, 'body'> & { body?: unknown; signal?: AbortSignal | null }

export async function request<T>(method: string, path: string, init: FetchInit = {}): Promise<T> {
  const headers = new Headers(init.headers)
  headers.set('accept', 'application/json')
  const isWrite = method !== 'GET' && method !== 'HEAD'
  if (isWrite) {
    if (init.body !== undefined) headers.set('content-type', 'application/json')
    // Without this the API answers 403 `csrf_failed`; the check exists because a
    // cross-site form post cannot set a custom header.
    if (csrfToken) headers.set(CSRF_HEADER, csrfToken)
  }
  let res: Response
  try {
    res = await fetch(`${API}${path}`, {
      method,
      credentials: 'include',
      ...init,
      headers,
      body: isWrite && init.body !== undefined ? JSON.stringify(init.body) : undefined,
    })
  } catch (e) {
    if ((e as Error)?.name === 'AbortError') throw e
    throw new ApiError(0, null, 'cannot reach the aerona api — is it running?')
  }
  const text = await res.text()
  let json: unknown = null
  if (text) {
    try {
      json = JSON.parse(text)
    } catch {
      json = { error: { code: 'bad_json', message: text.slice(0, 200) } }
    }
  }
  if (!res.ok) {
    // A 401 is not a fact about the request, it is a fact about the visitor, so it is also
    // announced: the session provider listens and re-reads `/me`, which is what turns "the
    // server revoked me while this page was open" into a bounce to the sign-in page instead of
    // a panel full of failing tiles. The API clears the cookie on the same response.
    if (res.status === 401 && typeof window !== 'undefined') {
      window.dispatchEvent(new Event('aerona:unauthorized'))
    }
    // A `null` body with a 401 still has to be distinguishable from a 500.
    throw new ApiError(res.status, (json as ApiErrorBody) ?? null, `request failed (${res.status})`)
  }
  return ((json as ApiEnvelope<T> | null)?.data ?? (json as T)) as T
}

export const get = <T,>(path: string, signal?: AbortSignal | null) => request<T>('GET', path, { signal })
export const post = <T,>(path: string, body?: unknown, signal?: AbortSignal | null) =>
  request<T>('POST', path, { body, signal })
export const patch = <T,>(path: string, body?: unknown, signal?: AbortSignal | null) =>
  request<T>('PATCH', path, { body, signal })
export const del = <T,>(path: string, signal?: AbortSignal | null) => request<T>('DELETE', path, { signal })
export const put = <T,>(path: string, body?: unknown, signal?: AbortSignal | null) =>
  request<T>('PUT', path, { body, signal })

// ---------------------------------------------------------------- identities

export type Me = {
  kind: 'session' | 'key'
  id: string
  username?: string
  csrf?: string
  guild?: string | null
  expiresAt?: number | null
  createdAt?: number | null
  ip?: string | null
  activeSessions?: number
  label?: string
  hint?: string
  scopes?: string[]
  lastUsedAt?: number | null
  requestCount?: number
}

export async function fetchMe(signal?: AbortSignal | null): Promise<Me | null> {
  try {
    const me = await get<Me>('/me', signal)
    // The API hands the CSRF secret to the session owner and nobody else; keeping it
    // in a module variable is what lets a page reload keep its session.
    setCsrf(me.kind === 'session' ? (me.csrf ?? null) : null)
    return me
  } catch (e) {
    if (e instanceof ApiError && e.unauthorized) {
      // No session, no csrf secret. Leaving the old one in the module would let a page still
      // believe it may write, and the server would answer that with a 403 nobody can explain.
      setCsrf(null)
      return null
    }
    throw e
  }
}

// The dashboard's only credential is a Discord session minted by `GET /auth/discord`.
// There used to be a `POST /pair` exchange for a code printed on the console; a second door
// brings a second set of failure modes, so it is gone rather than hidden.
export const logout = () => post<{ ok: boolean; selfRevoked: boolean }>('/logout')

// -------------------------------------------------------------------- guilds

export type GuildSummary = {
  id: string
  name: string
  icon: string | null
  memberCount: number | null
  botPresent: boolean
  openTickets: number
  stickies: number
  /** Present only when Aerona is not in this server: Discord's "add the bot" link for it,
   *  built by the API so the permissions the product needs are one list, in Rust. */
  inviteUrl?: string | null
}
export const guilds = () => get<{ guilds: GuildSummary[] }>('/guilds')

/** `GET /guilds/{guild}`: one call with everything the overview tile needs. */
export type GuildOverview = {
  guild: { id: string; addedAt: number | null; syncedAt: number | null }
  settings: Record<string, Record<string, unknown>>
  moderation: Record<string, unknown>
  tickets: { open?: number; pending?: number; closedLast7d?: number }
  aiRules: { total: number; enabled: number }
  raid: { active: boolean; event: RowValue | null }
  live: RowValue | null
}
type RowValue = Record<string, unknown>
export const guildOverview = (g: string) => get<GuildOverview>(`/guilds/${g}`)

export type Feature =
  | 'general'
  | 'moderation'
  | 'tickets'
  | 'sticky'
  | 'anti_raid'
  | 'ai_filter'
  | 'link_guard'
  | 'anti_spam'
  | 'identity'
  | 'autorole'
  | 'welcome'
  | 'logging'
  | 'commands'

export const settingsDoc = (g: string, f: Feature) => get<Record<string, unknown>>(`/guilds/${g}/settings/${f}`)
export type SettingsSave = {
  changed: boolean
  /** The stored document, defaults filled — adopt this rather than what you sent. */
  settings: Record<string, unknown>
  ignored?: string[]
  /**
   * The outbox row the save queued, when the document has a Discord-side consequence (the
   * bot's own profile, a command list). `null` means nothing was queued; absent means this
   * feature has no follow-up to have. A panel that says "Saved" for a nick the bot has not
   * pushed yet is only honest if it can also say the push is still owed.
   */
  queued?: number | null
}
export const patchSettings = (g: string, f: Feature, body: Record<string, unknown>) =>
  patch<SettingsSave>(`/guilds/${g}/settings/${f}`, body)
export type HistoryRow = { id: number; feature: string; changedBy: string; at: number; before: unknown; after: unknown; hash: string }
export const settingsHistory = (g: string, f: Feature) => get<{ history: HistoryRow[] }>(`/guilds/${g}/settings/${f}/history`)

// -------------------------------------------------------------- moderation

/** Warn rows are the `WarnRecord` struct, so they are snake_case. */
export type WarnRow = {
  id: number
  guild_id?: string
  user_id: string
  moderator_id: string
  reason: string
  created_at: number
  expires_at: number | null
  active: boolean
  pardoned_at?: number | null
  context_message?: string | null
}
export const warns = (g: string, user?: string, limit = 50) =>
  get<{ warns: WarnRow[]; counts: { active: number; strikes: number } | null }>(`/guilds/${g}/warns?limit=${limit}${user ? `&user=${user}` : ''}`)
export type WarnOutcome = { id: number; activeCount: number; strikes: number; escalated: string | null }
export const createWarn = (g: string, body: { user: string; reason: string; message?: string }) => post<WarnOutcome>(`/guilds/${g}/warns`, body)
export const pardonWarn = (g: string, id: number) => post<{ pardoned: boolean }>(`/guilds/${g}/warns/${id}/pardon`)
export const clearWarns = (g: string, user: string, count = 0) =>
  del<{ cleared: number }>(`/guilds/${g}/users/${user}/warns${count ? `?count=${count}` : ''}`)
export const modStats = (g: string) => get<Record<string, unknown>>(`/guilds/${g}/moderation/stats`)
export const topWarned = (g: string) => get<Record<string, unknown>>(`/guilds/${g}/top-warned`)
export type StrikeInfo = {
  user: string
  strikes: number
  activeWarns: number
  notes: { id: number; authorId: string; body: string; at: number }[]
  strikeThreshold: number
  escalateAction: string
}
export const strikes = (g: string, user: string) => get<StrikeInfo>(`/guilds/${g}/users/${user}/strikes`)
export const addNote = (g: string, user: string, body: string) => post<{ id: number }>(`/guilds/${g}/users/${user}/notes`, { body })
export const deleteNote = (g: string, id: number) => del<{ deleted: boolean }>(`/guilds/${g}/notes/${id}`)

/** A moderation action goes to the outbox, not to Discord: the API never holds a
 *  token, and a restart cannot lose a queued ban. */
export type Queued = { id: number; poll: string }
export type ActionsQueued = { queued: number[]; count: number; poll: string }
export const queueAction = (g: string, actions: Record<string, unknown>[]) => post<ActionsQueued>(`/guilds/${g}/actions`, { actions })
export type OutboxRow = {
  id: number
  status: 'pending' | 'claimed' | 'done' | 'failed'
  error: string | null
  reference: string | null
  action: string
  args: unknown
  createdAt: number
  doneAt: number | null
}
export const outboxStatus = (id: number | string) => get<OutboxRow>(`/outbox/${id}`)

// ------------------------------------------------------------------ tickets

export type Ticket = {
  id: number
  reference: string
  subject: string
  status: 'open' | 'pending' | 'closed'
  ownerId: string
  claimedBy: string | null
  channelId: string
  createdAt: number
  closedAt: number | null
  messageCount: number
}
export type TranscriptRow = { authorId: string; author: string; content: string; internal: boolean; at: number; seq: number }
export const tickets = (g: string, opts: { status?: string; limit?: number; offset?: number } = {}) => {
  const q = new URLSearchParams()
  if (opts.status) q.set('status', opts.status)
  q.set('limit', String(opts.limit ?? 50))
  if (opts.offset) q.set('offset', String(opts.offset))
  return get<{ tickets: Ticket[]; counts: { open: number; pending: number; closedLast7d: number } }>(`/guilds/${g}/tickets?${q}`)
}
export const openTicket = (g: string, body: { user: string; subject: string }) => post<Queued>(`/guilds/${g}/tickets`, body)
export const ticket = (g: string, id: number | string) => get<{ ticket: Ticket; transcript: TranscriptRow[] }>(`/guilds/${g}/tickets/${id}`)
export const setTicketStatus = (g: string, id: number, status: string) =>
  patch<Record<string, unknown>>(`/guilds/${g}/tickets/${id}`, { status })
export const claimTicket = (g: string, id: number, force = false) => post<{ claimed: boolean; ticket: Ticket }>(`/guilds/${g}/tickets/${id}/claim`, { force })
export const unclaimTicket = (g: string, id: number) => post<{ unclaimed: boolean }>(`/guilds/${g}/tickets/${id}/unclaim`)
export const ticketMessage = (g: string, id: number, content: string, internal = false) =>
  post<{ id: number; internal: boolean }>(`/guilds/${g}/tickets/${id}/messages`, { content, internal })
export const ticketTranscriptUrl = (g: string, id: number) => `${API}/guilds/${g}/tickets/${id}/transcript`

// ------------------------------------------------------------------ stickies

/** `StickyRecord` is serialised directly, so these keys are snake_case. */
export type StickyRow = {
  id: number
  channel_id: string
  message_id: string
  sticky_message_id: string | null
  content: string
  every_messages: number
  since_count: number
  created_by: string
  created_at: number
  last_run_at: number
}
export const stickies = (g: string) => get<{ stickies: StickyRow[]; channels: string[] }>(`/guilds/${g}/stickies`)
export const addSticky = (g: string, body: { channel: string; message: string; content: string; everyMessages: number }) =>
  post<{ id: number; stickies: StickyRow[] }>(`/guilds/${g}/stickies`, body)
export const deleteSticky = (g: string, id: number) => del<{ ok: boolean }>(`/guilds/${g}/stickies/${id}`)

// --------------------------------------------------------------------- raid

export type RaidEvent = {
  id: number
  started_at: number
  ended_at: number | null
  trigger: string
  peak: number
  window_secs: number
  flagged: number
  kicked: number
  banned: number
  actions: string
}
export const raidEvents = (g: string) =>
  get<{ events: RaidEvent[]; active: RaidEvent | null; config: Record<string, unknown> }>(`/guilds/${g}/raid-events`)
/** `active: false` is the whole point of the second call: the endpoint takes one shape for
 *  freezing and unfreezing, and a body that omitted it would silently re-lock instead. */
export const raidLockdown = (g: string, minutes: number, reason: string, active = true) =>
  post<ActionsQueued>(`/guilds/${g}/raid-lockdown`, { active, minutes, reason })

// --------------------------------------------------------------------- AI filter

export type Rule = {
  id: number
  pattern: string
  is_regex: boolean
  mode: 'whole_word' | 'substring' | 'regex'
  category: string
  severity: 'info' | 'low' | 'medium' | 'high' | 'critical'
  action: 'flag' | 'warn' | 'delete' | 'delete_strike' | 'delete_kick' | 'delete_ban'
  strikes: number
  description: string
  enabled: boolean
  hits: number
}
export const rules = (g: string) => get<{ rules: Rule[] }>(`/guilds/${g}/ai-filter/rules`)
export const putRule = (g: string, body: Record<string, unknown>) => post<{ id: number; rules: Rule[] }>(`/guilds/${g}/ai-filter/rules`, body)
export const aiEventRows = (g: string, limit = 25) => get<Record<string, unknown>>(`/guilds/${g}/ai-events?limit=${limit}`)
export const patchRule = (g: string, id: number, enabled: boolean) => patch<{ rules: Rule[] }>(`/guilds/${g}/ai-filter/rules/${id}`, { enabled })
export const deleteRule = (g: string, id: number) => del<{ rules: Rule[] }>(`/guilds/${g}/ai-filter/rules/${id}`)
export type FilterTest = {
  blocked: boolean
  verdict: { action: string; severity: string; reason: string; strikes: number; categories: string[] }
  provider: { called: boolean; note?: string }
  effectiveThresholds: Record<string, number>
  rulesCompared: number
  echo?: { text: string; hits: unknown[] }
}
export const testFilter = (g: string, text: string, channel = '') =>
  post<FilterTest>(`/guilds/${g}/ai-filter/test`, { text, channel })
export const aiEvents = (g: string, limit = 25) => get<Record<string, unknown>>(`/guilds/${g}/ai-events?limit=${limit}`)

// --------------------------------------------------------------- levels, misc

export type LevelRow = { userId: string; level: number; xp: number; messages: number; name: string }
export const levels = (g: string, limit = 25) =>
  get<{ leaderboard: LevelRow[]; enabled: boolean; curve: { xpForLevel: number[] } }>(`/guilds/${g}/levels?limit=${limit}`)

export type AuditEntry = {
  id: number
  at: number
  actorType: 'user' | 'apikey' | 'bot' | 'system'
  actorId: string | null
  actorLabel: string | null
  action: string
  target: string | null
  ip: string | null
  meta: unknown
  guildId?: string | null
  hash?: string
}
export const auditList = (params: Record<string, string> = {}) => {
  const q = new URLSearchParams(params).toString()
  return get<{ entries: AuditEntry[]; nextBefore: number | null; chain: boolean }>(`/audit${q ? `?${q}` : ''}`)
}
export const verifyAudit = () => post<Record<string, unknown>>('/audit/verify')

export type ApiKey = {
  id: number
  label: string
  hint: string
  guild: string | null
  scopes: string[]
  createdAt: number
  lastUsedAt: number | null
  expiresAt: number | null
  expired: boolean
  revokedAt: number | null
  requestCount: number
}
export const keys = () => get<{ keys: ApiKey[] }>('/keys')
export const createKey = (body: { label: string; scopes: string[]; guild?: string | null; ttlDays?: number | null }) =>
  post<{ id: number; key: string; hint: string; scopes: string[]; warning: string }>('/keys', body)
export const revokeKey = (id: number) => del<{ ok: boolean }>(`/keys/${id}`)

export type Session = {
  id: number
  current: boolean
  createdAt: number
  lastSeenAt: number
  expiresAt: number
  ip: string | null
  userAgent: string | null
  guild: string | null
}
export const sessions = () => get<{ sessions: Session[] }>('/sessions')
export const revokeSession = (id: number) => del<{ ok: boolean }>(`/sessions/${id}`)
export const revokeAllSessions = () => post<{ ok: boolean; selfRevoked: boolean; revoked: number }>('/sessions/revoke-all')

/** `GET /status` is public and is what the supervisor probes; the app reads it too. */
export type Status = {
  version: string
  apiVersion: string
  uptimeSecs: number
  guilds: number
  db: { sizeKb: number }
  queue: { pending: number; failed: number }
  bot: { wsLatencyMs: number | null; rssKb: number | null; messagesPerMin: number | null; reportingAt: number; stale: boolean } | null
  features: { aiFilter: boolean; discordOauth: boolean; contentIntent: boolean }
  /**
   * What a Discord login would send as `redirect_uri`, and where that value came from.
   * `redirectUri` is `null` when the answer is "whichever origin you are looking at", because
   * echoing a `Host`-derived URL back through JSON would let this endpoint be used as an
   * origin oracle; the browser already knows its own origin, so the page builds it locally.
   */
  oauth?: {
    configured: boolean
    redirectUriSource: 'pinned' | 'base-url' | 'request-host' | 'unset' | string
    redirectUri: string | null
    callbackPath: string
    guidance?: string[]
  }
  requestId?: string
}
export const status = () => get<Status>('/status')
export const stats = () => get<Record<string, unknown>>('/stats')
export const statsSeries = (process: 'bot' | 'api', points = 24) =>
  get<Record<string, unknown>>(`/stats/${process}?points=${Math.max(2, Math.min(2000, Math.round(points)))}`)
export const housekeeping = () => post<Record<string, unknown>>('/housekeeping')
export const manifest = () => get<Record<string, unknown>>('/manifest')
export const health = () => fetch(`${API}/healthz`, { method: 'GET' }).then((r) => r.ok)

// ------------------------------------------------- reports from the gateway worker
//
// Three things the dashboard shows but does not own: the slash commands Discord has
// registered for this guild, the channel/role names worth offering in a picker, and the
// result of the last attempt to set this guild's copy of the bot profile. The bot writes
// them as it learns them, so every field here is "as of `reportedAt`", never live. A
// worker that has never reported answers `null` and the page says so.
export type CommandRow = {
  id?: string
  name: string
  description?: string
  enabled?: boolean
  staffOnly?: boolean
  guildScoped?: boolean
}
export type PickerOption = { id: string; name: string }
export type GuildReport = {
  /** What Discord accepted, as the worker last saw it. Every row is `reportedAt`, never now. */
  commands?: { reportedAt?: number; count?: number; commands?: CommandRow[]; disabled?: string[] } | null
  /** Channels and roles, from the gateway cache, for the pickers on every other page. */
  picker?: { reportedAt?: number; channels?: PickerOption[]; roles?: PickerOption[] } | null
  /** The last attempt to push this guild's copy of the bot's profile to Discord. */
  identity?: { reportedAt?: number; ok?: boolean; error?: string | null; skipped?: string; fields?: string[] } | null
  guildId?: string
}
export const guildReport = (g: string, signal?: AbortSignal | null) =>
  get<GuildReport>(`/guilds/${g}/report`, signal).catch((e) => {
    // An install whose worker has not written a report yet is normal, not an error to
    // show: the pages that read it render "not reported yet" from a null.
    if (e instanceof ApiError && (e.status === 404 || e.unauthorized)) return { } as GuildReport
    throw e
  })

/** Ask the worker to (re)register this guild's slash commands. Queued, not awaited: the
 *  outbox is the only thing allowed to talk to Discord, and a slow Discord must not hang a
 *  dashboard request. */
export const syncCommands = (g: string) => post<{ queued: number; poll: string }>(`/guilds/${g}/commands/sync`)
export const applyIdentity = (g: string) => post<{ queued: number; poll: string }>(`/guilds/${g}/identity/apply`)

/** Stage an avatar for the next apply. The bytes go here, not into the settings document:
 *  that document is copied whole into `config_history` on every save, and a megabyte of
 *  base64 in each row would make the audit drawer unreadable. */
export const stageAvatar = (g: string, dataUri: string) =>
  put<{ staged: boolean; kind: string; bytes: number; hash: string }>(`/guilds/${g}/identity/avatar`, { data: dataUri })
export const clearAvatar = (g: string) => del<{ cleared: boolean }>(`/guilds/${g}/identity/avatar`)
