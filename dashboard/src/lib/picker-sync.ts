/**
 * Ask the worker for fresh channel, role and message lists, and wait for the answer.
 *
 * The bot reads its queue every two seconds, so a refresh lands in a few polls instead of on the
 * hour-long pass it used to wait for. Waiting on `reportedAt` rather than on a fixed delay is the
 * difference between a control that tells the truth and a spinner that stops early: the number
 * only moves once the worker has actually been to Discord.
 */
import { guildReport, pickerRefresh } from './api'

/** True when the worker answered. False means "not yet", which a caller should say out loud
 *  rather than leave as an unchanged list that looks like an empty server. */
export async function refreshPicker(
  guild: string,
  base: number | undefined,
  reload: () => unknown,
): Promise<boolean> {
  try {
    await pickerRefresh(guild)
  } catch {
    return false
  }
  for (let poll = 0; poll < 10; poll += 1) {
    await new Promise((r) => setTimeout(r, poll < 4 ? 1000 : 2000))
    const after = await guildReport(guild)
    if ((after?.picker?.reportedAt ?? 0) > (base ?? 0)) {
      await reload()
      return true
    }
  }
  return false
}
