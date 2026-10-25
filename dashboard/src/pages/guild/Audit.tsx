/**
 * The audit trail for this install, newest first.
 *
 * Every row carries a hash over the row before it, which is why the verify button exists: it
 * recomputes the chain and says whether anything was rewritten. A drawer you can edit silently
 * is a drawer nobody should have to trust. Verification is a full table walk, so it happens
 * when you press the button and never as a side effect of opening the page.
 */
import { useState } from 'react'
import { toast } from 'sonner'
import { CircleCheck, ShieldAlert } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { TableCell } from '@/components/ui/table'

import { ApiError, auditList, verifyAudit, type AuditEntry } from '../../lib/api'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { ago, PageHeader, Panel, shortId, Table } from './shared'

export default function Audit() {
  const { id } = useGuild()
  // A stack of cursors rather than a page number: `before` is an id, and the only honest way
  // to go back is the value the server handed over when it gave you this page.
  const [cursors, setCursors] = useState<(number | null)[]>([null])
  const at = cursors[cursors.length - 1] ?? null
  const list = useAsync((signal) => auditList({ limit: '80', guild: id, ...(at ? { before: String(at) } : {}) }), [id, at])
  const rows = (list.data?.entries ?? []) as AuditEntry[]
  const next = list.data?.nextBefore ?? null
  const [chain, setChain] = useState<'unknown' | 'intact' | 'broken'>('unknown')
  const [looking, setLooking] = useState(false)

  const verify = async () => {
    setLooking(true)
    try {
      const res = await verifyAudit()
      setChain(res.intact === false ? 'broken' : 'intact')
      toast.success(`Chain verified · ${String(res.verifiedRows ?? 0)} rows`)
    } catch (e) {
      setChain('broken')
      toast.error(e instanceof ApiError ? e.message : 'Verification failed.')
    } finally {
      setLooking(false)
    }
  }

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Audit"
        hint="Configuration and moderation actions taken through this panel."
        actions={
          <>
            {chain !== 'unknown' ? (
              <Badge variant={chain === 'intact' ? 'outline' : 'destructive'} className="gap-1">
                {chain === 'intact' ? <CircleCheck className="size-3" /> : <ShieldAlert className="size-3" />}
                {chain === 'intact' ? 'chain intact' : 'chain broken'}
              </Badge>
            ) : null}
            <Button size="sm" variant="outline" onClick={verify} disabled={looking}>
              {looking ? 'Checking…' : 'Verify chain'}
            </Button>
          </>
        }
      />

      <Panel
        title={cursors.length === 1 ? 'Entries' : `Entries · page ${cursors.length}`}
        hint={`${rows.length} shown, this server only`}
        actions={
          cursors.length > 1 ? (
            <Button variant="ghost" size="sm" onClick={() => setCursors((c) => c.slice(0, -1))}>
              Newer
            </Button>
          ) : null
        }
      >
        <Table
          head={['When', 'Who', 'Action', 'Target', 'From', 'Hash']}
          rows={rows}
          render={(raw) => {
            const e = raw as AuditEntry
            return (
              <>
                <TableCell className="whitespace-nowrap text-muted-foreground">{ago(e.at)}</TableCell>
                <TableCell className="max-w-[24ch] truncate" title={e.actorId ?? ''}>
                  {e.actorLabel ?? e.actorType}
                  {e.actorType === 'apikey' ? (
                    <Badge variant="outline" className="ms-2">
                      key
                    </Badge>
                  ) : null}
                </TableCell>
                <TableCell>
                  <code className="text-[13px]">{e.action}</code>
                </TableCell>
                <TableCell className="text-muted-foreground" title={e.target ?? ''}>
                  {e.target ? shortId(e.target) : '—'}
                </TableCell>
                <TableCell className="text-muted-foreground">{e.ip ?? '—'}</TableCell>
                <TableCell className="font-mono text-[11px] text-muted-foreground" title={e.hash ?? ''}>
                  {e.hash ? e.hash.slice(0, 10) : '—'}
                </TableCell>
              </>
            )
          }}
        />
        {next ? (
          <Button variant="outline" size="sm" onClick={() => setCursors((c) => [...c, Number(next)])}>
            Older
          </Button>
        ) : (
          <p className="text-xs text-muted-foreground">
            {cursors.length === 1 ? 'Nothing older than this in the log.' : 'That is the beginning of the log.'}
          </p>
        )}
      </Panel>
    </div>
  )
}
