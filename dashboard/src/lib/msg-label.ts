/**
 * One line that says what a message was.
 *
 * A sticky is anchored to a message id nobody can read, so every picker that points at a message
 * shows the sentence instead. The text and the channel both come from the worker's report: the
 * cache carries the channel id, and the name comes from the channel list in the same report.
 */
import type { GuildReport, RecentMessage } from './api'

/** `#channel: what it said`, shortened to what fits a row. */
export function describeMessage(report: GuildReport | null | undefined, m: RecentMessage): string {
  const body = (m.content ?? '').replace(/\s+/g, ' ').trim()
  const channel = report?.picker?.channels?.find((c) => c.id === m.channelId)?.name
  return `${channel ? `#${channel}` : 'a message'}: ${body ? body.slice(0, 90) : '(no text)'}`
}

/** The messages the worker reported, newest first, narrowed to one channel and without the repeats
 *  an edited row can leave in the cache. */
export function messageChoices(
  report: GuildReport | null | undefined,
  channelId?: string,
): RecentMessage[] {
  const rows = report?.messages ?? []
  const seen = new Set<string>()
  const out: RecentMessage[] = []
  for (const m of rows) {
    if (channelId && m.channelId !== channelId) continue
    if (seen.has(m.id)) continue
    seen.add(m.id)
    out.push(m)
  }
  return out
}
