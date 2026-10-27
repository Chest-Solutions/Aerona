/**
 * The one page between "a browser on this box" and "the dashboard".
 *
 * It has two jobs and neither is explaining anything: send the browser to Discord, and if
 * Discord refuses, say which refusal it was - in one sentence, next to the button that fixes
 * it. The redirect-URI diagnostics used to live here; they went to `python3 main.py check`,
 * which is where a value that has to match character for character belongs: on the console,
 * beside the file that sets it.
 *
 * `?next=` is preserved by hand through the bounce, because the server is what stores it
 * against the OAuth state - see `crate::oauth::safe_next`.
 */
import { useEffect, useState } from 'react'
import { motion } from 'framer-motion'

import { Alert, AlertDescription } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'

import { status as statusApi } from '../lib/api'
import { useSession } from '../lib/hooks'
import { useRouter } from '../router'
import { pagesMeta } from '../pages-meta'
import { ThemeToggle } from '../components/theme-toggle'
import { Brand } from '../components/brand'

export const meta = pagesMeta.Login

const AUTHORIZE = '/api/v1/auth/discord'

/** Discord's `error` query parameter, translated. The values are fixed strings from the
 *  OAuth spec plus the ones this API raises itself, and anything unrecognised falls back to
 *  the generic sentence - the parameter is attacker-reachable text, so nothing here is ever
 *  echoed, only compared. */
function explain(error: string | null): string | null {
  if (!error) return null
  const code = error.replace(/^discord-/, '').replace(/_/g, '-')
  switch (code) {
    case 'access-denied':
      return 'You closed the consent screen before choosing a server. Nothing was authorized.'
    case 'no-admin-guild':
      return 'This account does not administer a server. Add Aerona to one you own, or take Administrator on one, then sign in again.'
    case 'expired-link':
    case 'invalid-request':
      return 'That link has been used or has gone stale. Start again.'
    case 'malformed-callback':
      return 'Discord came back without a code, which usually means this address was opened on its own. Sign in again from the start.'
    case 'discord-timeout':
      return 'Discord did not answer in time. If this box cannot reach discord.com, the sign-in cannot finish.'
    case 'discord-unreachable':
    case 'discord-refused':
      return 'Discord refused to complete the sign-in. Check the client id and secret in .env, then run python3 main.py check.'
    case 'oauth-unconfigured':
      return 'No Discord application yet. AERONA_DISCORD_CLIENT_ID and AERONA_DISCORD_CLIENT_SECRET go in .env, then restart.'
    case 'origin-unset':
      return 'This install does not know which address it is reached by. Set AERONA_BASE_URL to exactly what the browser types.'
    default:
      return 'Discord did not complete the sign-in. Start again, and if it repeats, run python3 main.py check.'
  }
}

export default function Login() {
  const { me, ready, refresh } = useSession()
  const { navigate, search } = useRouter()
  const [configured, setConfigured] = useState<boolean | null>(null)
  const note = explain(search.get('error'))
  // Where the browser was heading before it was bounced. The API reads it off this query
  // string when the sign-in starts, so the return trip lands inside the server it left.
  const next = search.get('next')

  useEffect(() => {
    let live = true
    statusApi()
      .then((s) => live && setConfigured(!!s.oauth?.configured))
      .catch(() => live && setConfigured(false))
    return () => {
      live = false
    }
  }, [])

  // A session already in the cookie means the work here is done. Both hops are `replace`, so
  // Back returns past the bounce instead of into it.
  useEffect(() => {
    if (ready && me) navigate(next && next.startsWith('/') ? next : '/', { replace: true })
  }, [ready, me, next, navigate])

  return (
    <div className="flex min-h-svh flex-col">
      <header className="flex h-14 items-center justify-between gap-3 px-4">
        <Brand size={26} />
        <ThemeToggle />
      </header>
      <div className="flex flex-1 items-start justify-center px-5 pb-16">
        <motion.div
          className="w-full max-w-[46ch]"
          initial={{ opacity: 0, y: 10 }}
          animate={{ opacity: 1, y: 0 }}
          transition={{ duration: 0.3, ease: [0.22, 1, 0.36, 1] }}
        >
          <Card>
            <CardHeader>
              <CardTitle>{configured === false ? 'No Discord application yet' : 'Sign in to see your servers'}</CardTitle>
              <CardDescription>
                {configured === false
                  ? 'The dashboard is behind Discord. Two variables in .env open it.'
                  : 'No password. Discord says which servers you administer, and this lists those.'}
              </CardDescription>
            </CardHeader>
            <CardContent className="grid gap-4">
              {note ? (
                <Alert variant="destructive">
                  <AlertDescription>{note}</AlertDescription>
                </Alert>
              ) : null}
              <div className="flex flex-wrap items-center gap-3">
                {configured ? (
                  <Button render={<a href={next ? `${AUTHORIZE}?next=${encodeURIComponent(next)}` : AUTHORIZE} />}>
                    Sign in with Discord
                  </Button>
                ) : configured === null ? (
                  <span className="text-sm text-muted-foreground">Checking this install…</span>
                ) : (
                  <>
                    <Button variant="outline" onClick={() => void refresh()}>
                      Try again
                    </Button>
                    <code className="rounded-md bg-muted px-2 py-1 text-xs">python3 main.py check</code>
                  </>
                )}
              </div>
              {next && next !== '/' ? (
                <p className="text-sm text-muted-foreground">
                  You were heading to <code className="text-xs">{next}</code>. That is where Discord puts you back.
                </p>
              ) : null}
            </CardContent>
          </Card>
        </motion.div>
      </div>
    </div>
  )
}
