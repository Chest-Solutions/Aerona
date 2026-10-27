/**
 * Who and what can drive this install: browser sessions and API keys.
 *
 * Keys are shown once and never again - the row keeps a hint and a hash, not a secret. That
 * is the whole design, and it is why revoking is cheap: a key you cannot read is a key nobody
 * else can read either.
 */
import { useState } from 'react'
import { AnimatePresence, motion } from 'framer-motion'
import { Key, Trash2, CircleX } from 'lucide-react'
import { toast } from 'sonner'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { TableCell } from '@/components/ui/table'

import {
  ApiError,
  createKey,
  guilds as fetchGuilds,
  keys as fetchKeys,
  revokeAllSessions,
  revokeKey,
  revokeSession,
  sessions as fetchSessions,
  type ApiKey,
  type Session,
} from '../lib/api'
import { useAsync, useSession } from '../lib/hooks'
import { Link } from '../router'
import { ThemeToggle } from '../components/theme-toggle'
import { PageHeader, Panel, Table, ago, shortId } from './guild/shared'

const SCOPES = ['read', 'moderate', 'config', 'tickets']

export default function Access() {
  const { me } = useSession()
  return (
    <div className="min-h-svh">
      <header className="sticky top-0 z-10 flex h-14 items-center gap-2 border-b bg-background/80 px-4 backdrop-blur">
        <Link href="/" className="text-sm font-medium">
          Servers
        </Link>
        <span className="text-muted-foreground">/</span>
        <span className="text-sm text-muted-foreground">Access</span>
        <span className="ms-auto flex items-center gap-3">
          {me ? <span className="hidden text-xs text-muted-foreground sm:inline">{me.username ?? me.kind}</span> : null}
          <ThemeToggle />
        </span>
      </header>
      <main className="mx-auto w-full max-w-5xl px-5 py-6">
        <div className="grid gap-5">
          <PageHeader title="Access" hint="Browser sessions, and the keys that let a script do what this panel does." />
          <Keys />
          <Sessions />
        </div>
      </main>
    </div>
  )
}

function Keys() {
  const list = useAsync((signal) => fetchKeys(), [])
  const rows = (list.data?.keys ?? []) as ApiKey[]
  const all = useAsync((signal) => fetchGuilds(), [])
  const [label, setLabel] = useState('')
  const [scopes, setScopes] = useState<string[]>(['read'])
  const [guild, setGuild] = useState('')
  const [ttl, setTtl] = useState('90')
  const [secret, setSecret] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const create = async () => {
    if (!label.trim() || scopes.length === 0) {
      toast.error('A label and at least one scope.')
      return
    }
    setBusy(true)
    try {
      const res = await createKey({
        label: label.trim(),
        scopes,
        guild: guild || null,
        ttlDays: Number(ttl) || null,
      })
      setSecret(res.key)
      setLabel('')
      list.reload()
      toast.success(res.warning)
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  const drop = async (k: ApiKey) => {
    try {
      await revokeKey(k.id)
      toast.success('Revoked')
      list.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'Could not revoke it.')
    }
  }

  return (
    <Panel title="API keys" hint={list.loading ? 'Loading…' : `${rows.filter((k) => !k.revokedAt && !k.expired).length} live`}>
      <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_auto_auto_auto] sm:items-end">
        <Label className="grid gap-1.5">
          <span className="text-sm font-medium">Label</span>
          <Input value={label} onChange={(e) => setLabel(e.target.value)} placeholder="ci deploy" />
        </Label>
        <Label className="grid gap-1.5">
          <span className="text-sm font-medium">Expires in</span>
          <Input className="w-24" value={ttl} onChange={(e) => setTtl(e.target.value)} inputMode="numeric" />
        </Label>
        <div className="grid gap-1.5">
          <span className="text-sm font-medium">One server</span>
          <Select value={guild} onValueChange={(v) => setGuild(String(v ?? ''))}>
            <SelectTrigger size="sm" className="w-44">
              <SelectValue placeholder="Every server" />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="">Every server</SelectItem>
              {(all.data?.guilds ?? []).map((g) => (
                <SelectItem key={g.id} value={g.id}>
                  {g.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <Button onClick={create} disabled={busy}>
          Create
        </Button>
      </div>

      <div className="flex flex-wrap gap-2">
        {SCOPES.map((s) => {
          const on = scopes.includes(s)
          return (
            <Button
              key={s}
              type="button"
              size="sm"
              variant={on ? 'default' : 'outline'}
              onClick={() => setScopes((cur) => (on ? cur.filter((x) => x !== s) : [...cur, s]))}
            >
              {s}
            </Button>
          )
        })}
      </div>

      <AnimatePresence>
        {secret ? (
          <motion.div initial={{ opacity: 0, y: -6 }} animate={{ opacity: 1, y: 0 }} exit={{ opacity: 0 }} className="rounded-xl border border-dashed p-3">
            <p className="text-sm font-medium">Copy it now - this is the only time it is shown.</p>
            <code className="mt-2 block select-all overflow-x-auto rounded-lg bg-muted/50 px-3 py-2 font-mono text-xs">{secret}</code>
            <div className="mt-2 flex gap-2">
              <Button size="sm" variant="outline" onClick={() => void navigator.clipboard.writeText(secret).then(() => toast.success('Copied'))}>
                Copy
              </Button>
              <Button size="sm" variant="ghost" onClick={() => setSecret(null)}>
                Done
              </Button>
            </div>
          </motion.div>
        ) : null}
      </AnimatePresence>

      <Table
        head={['Label', 'Scopes', 'Server', 'Created', 'Last used', 'Requests', '']}
        rows={rows}
        render={(raw) => {
          const k = raw as ApiKey
          const dead = Boolean(k.revokedAt) || k.expired
          return (
            <>
              <TableCell className="font-medium">
                {k.label}
                <span className="ms-2 font-mono text-[11px] text-muted-foreground">{k.hint}</span>
              </TableCell>
              <TableCell>
                <span className="flex flex-wrap gap-1">
                  {k.scopes.map((s) => (
                    <Badge key={s} variant="outline">
                      {s}
                    </Badge>
                  ))}
                </span>
              </TableCell>
              <TableCell className="text-muted-foreground" title={k.guild ?? ''}>
                {k.guild ? shortId(k.guild) : 'all'}
              </TableCell>
              <TableCell className="text-muted-foreground">{ago(k.createdAt)}</TableCell>
              <TableCell className="text-muted-foreground">{k.lastUsedAt ? ago(k.lastUsedAt) : 'never'}</TableCell>
              <TableCell className="tabular-nums">{k.requestCount}</TableCell>
              <TableCell className="text-end">
                {dead ? (
                  <Badge variant="outline">{k.expired ? 'expired' : 'revoked'}</Badge>
                ) : (
                  <Button variant="ghost" size="sm" onClick={() => void drop(k)}>
                    <Trash2 className="size-4" />
                  </Button>
                )}
              </TableCell>
            </>
          )
        }}
      />
    </Panel>
  )
}

function Sessions() {
  const list = useAsync((signal) => fetchSessions(), [])
  const rows = (list.data?.sessions ?? []) as Session[]
  const { refresh } = useSession()

  const drop = async (s: Session) => {
    try {
      await revokeSession(s.id)
      toast.success(s.current ? 'Signed out of this browser too' : 'Revoked')
      list.reload()
      if (s.current) void refresh()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'Could not revoke it.')
    }
  }

  const dropAll = async () => {
    try {
      const res = await revokeAllSessions()
      toast.success(`${res.revoked} revoked`)
      list.reload()
      if (res.selfRevoked) void refresh()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'Could not revoke them.')
    }
  }

  return (
    <Panel
      title="Signed-in browsers"
      hint={list.loading ? 'Loading…' : `${rows.length} active`}
      actions={
        rows.length > 1 ? (
          <Button variant="outline" size="sm" onClick={dropAll}>
            <CircleX className="size-4" /> Revoke all
          </Button>
        ) : null
      }
    >
      <Table
        head={['Device', 'Server', 'From', 'Seen', 'Expires', '']}
        rows={rows}
        render={(raw) => {
          const s = raw as Session
          return (
            <>
              <TableCell className="max-w-[42ch] truncate" title={s.userAgent ?? ''}>
                <span className="inline-flex items-center gap-2">
                  <Key className="size-3.5 text-muted-foreground" />
                  {s.userAgent ? s.userAgent.slice(0, 60) : 'unknown device'}
                </span>
                {s.current ? <Badge variant="secondary" className="ms-2">this one</Badge> : null}
              </TableCell>
              <TableCell className="text-muted-foreground" title={s.guild ?? ''}>
                {s.guild ? shortId(s.guild) : 'all'}
              </TableCell>
              <TableCell className="text-muted-foreground">{s.ip ?? '-'}</TableCell>
              <TableCell className="text-muted-foreground">{ago(s.lastSeenAt)}</TableCell>
              <TableCell className="text-muted-foreground">{new Date(s.expiresAt * 1000).toLocaleString()}</TableCell>
              <TableCell className="text-end">
                <Button variant="ghost" size="sm" onClick={() => void drop(s)}>
                  Revoke
                </Button>
              </TableCell>
            </>
          )
        }}
      />
    </Panel>
  )
}
