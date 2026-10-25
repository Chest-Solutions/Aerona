/**
 * Tickets: the private channel a member gets when they need help, and the record of it.
 *
 * The board below is the same table the bot's `/ticket` command writes to, so a claim made
 * here is a claim the bot honours in Discord — one queue, two doors.
 */
import { toast } from 'sonner'
import { Ticket as TicketGlyph } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { TableCell } from '@/components/ui/table'

import { ApiError, claimTicket, setTicketStatus, tickets, unclaimTicket, ticketTranscriptUrl, type Ticket } from '../../lib/api'
import { useAsync } from '../../lib/hooks'
import { useGuild } from '../../lib/guild'
import { SettingsForm } from '../../lib/settings-form'
import { ago, PageHeader, Panel, shortId, Table } from './shared'

export default function Tickets() {
  const { id, settings } = useGuild()
  const cfg = settings('tickets') as Record<string, unknown>
  const list = useAsync((signal) => tickets(id, { status: '', limit: 50 }), [id])
  const rows = (list.data?.tickets ?? []) as Ticket[]
  const counts = list.data?.counts ?? null

  const act = async (label: string, fn: () => Promise<unknown>) => {
    try {
      await fn()
      toast.success(label)
      list.reload()
    } catch (e) {
      toast.error(e instanceof ApiError ? e.message : 'The API did not answer.')
    }
  }

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Tickets"
        hint="A private channel per request, with a transcript when it closes."
        actions={
          counts ? (
            <>
              <Badge variant="secondary">{counts.open} open</Badge>
              <Badge variant="outline">{counts.pending} pending</Badge>
            </>
          ) : null
        }
      />

      {!cfg.enabled ? (
        <p className="flex items-center gap-2 rounded-xl border border-dashed px-4 py-3 text-sm text-muted-foreground">
          <TicketGlyph className="size-4" /> Tickets are switched off, so nothing below will fill up until you turn them on.
        </p>
      ) : null}

      <Panel
        title="Board"
        hint={list.loading ? 'Loading…' : `${rows.length} recent · newest ${ago(rows[0]?.createdAt)}`}
        actions={
          <Button variant="outline" size="sm" onClick={list.reload} disabled={list.loading}>
            Reload
          </Button>
        }
      >
        <Table
          head={['Ref', 'Subject', 'Opened by', 'Claimed', 'Status', 'Msgs', '']}
          rows={rows}
          render={(raw) => {
            const t = raw as Ticket
            return (
              <>
                <TableCell className="font-medium">{t.reference}</TableCell>
                <TableCell className="max-w-[30ch] truncate" title={t.subject}>
                  {t.subject}
                </TableCell>
                <TableCell className="text-muted-foreground" title={t.ownerId}>
                  {shortId(t.ownerId)}
                </TableCell>
                <TableCell className="text-muted-foreground" title={t.claimedBy ?? ''}>
                  {t.claimedBy ? shortId(t.claimedBy) : '—'}
                </TableCell>
                <TableCell>
                  <Badge variant={t.status === 'open' ? 'default' : t.status === 'pending' ? 'secondary' : 'outline'}>
                    {t.status}
                  </Badge>
                </TableCell>
                <TableCell className="tabular-nums">{t.messageCount}</TableCell>
                <TableCell className="text-end whitespace-nowrap">
                  {t.status !== 'closed' ? (
                    t.claimedBy ? (
                      <Button variant="ghost" size="sm" onClick={() => void act('Released', () => unclaimTicket(id, t.id))}>
                        Release
                      </Button>
                    ) : (
                      <Button variant="ghost" size="sm" onClick={() => void act('Claimed', () => claimTicket(id, t.id))}>
                        Claim
                      </Button>
                    )
                  ) : null}
                  {t.status !== 'closed' ? (
                    <Button variant="ghost" size="sm" onClick={() => void act('Closed', () => setTicketStatus(id, t.id, 'closed'))}>
                      Close
                    </Button>
                  ) : (
                    <Button variant="ghost" size="sm" render={<a href={ticketTranscriptUrl(id, t.id)} />} >
                      Transcript
                    </Button>
                  )}
                </TableCell>
              </>
            )
          }}
        />
      </Panel>

      <SettingsForm
        feature="tickets"
        title="How tickets are made"
        icon={TicketGlyph}
        description="The bot creates the channel itself; you decide where it lands and who sees it."
        fields={[
          { kind: 'switch', key: 'enabled', label: 'Turn tickets on', hint: 'Adds /ticket to this server.' },
          { kind: 'ids', key: 'category_id', label: 'Category', hint: 'Ticket channels are created under it.', source: 'channels' },
          { kind: 'ids', key: 'support_role_id', label: 'Staff role', hint: 'Pingable on a new ticket, and able to see the channel.', source: 'roles' },
          { kind: 'ids', key: 'transcript_channel_id', label: 'Transcripts to', hint: 'Where the closing transcript is posted.', source: 'channels' },
          { kind: 'text', key: 'name_format', label: 'Channel name', hint: 'Tokens: {ref} {user} {subject}.', placeholder: 'ticket-{ref}-{user}' },
          { kind: 'int', key: 'max_per_user', label: 'Open per member', min: 1, max: 10, suffix: 'tickets' },
          { kind: 'int', key: 'auto_close_days', label: 'Auto-close after', hint: '0 leaves them open until staff act.', min: 0, max: 90, suffix: 'days idle' },
          { kind: 'switch', key: 'claim_required', label: 'Answering requires a claim', hint: 'Stops two staff replying at once.' },
          { kind: 'switch', key: 'show_claim', label: 'Give staff a Claim button', hint: 'Off means claiming only through /ticket claim.' },
          { kind: 'switch', key: 'show_close', label: 'Give staff a Close button', hint: 'Buttons live in the ticket channel, not only in this panel.' },
          { kind: 'switch', key: 'ping_support_role', label: 'Ping the staff role on open' },
          { kind: 'switch', key: 'use_two_channels', label: 'Private channel plus a public one', hint: 'The public one is where the member writes; the private one is staff-only.' },
          { kind: 'textarea', key: 'default_message', label: 'First message in the channel', rows: 2, max: 500 },
          {
            kind: 'enum',
            key: 'transcript_format',
            label: 'Transcript file',
            options: [
              { value: 'text', label: 'Plain text' },
              { value: 'json', label: 'JSON' },
            ],
          },
        ]}
      />
    </div>
  )
}
