/**
 * The bot's face in this server only.
 *
 * Discord keeps nickname, avatar and bio on the *guild member*, so what is set here changes
 * nothing for anybody else's server, and clearing a field hands control back to the account
 * profile. The avatar travels as a data URI because this route only accepts image bytes —
 * the file never reaches disk: it is read in the browser and handed to the API.
 */
import { useRef, useState } from 'react'
import { BadgeCheck, CircleCheck, CircleX, Upload } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Label } from '@/components/ui/label'

import { ApiError, applyIdentity, clearAvatar, stageAvatar } from '../../lib/api'
import { useGuild } from '../../lib/guild'
import { ago, PageHeader } from './shared'
import { SettingsForm } from '../../lib/settings-form'

const TYPES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp']
const MAX_BYTES = 900_000

export default function Profile() {
  const { id, settings, report } = useGuild()
  const cfg = settings('identity') as Record<string, unknown>
  const status = report?.identity ?? null
  const file = useRef<HTMLInputElement>(null)
  const [note, setNote] = useState<string | null>(null)
  // The data URI lives here and nowhere else: it is what the preview shows before anything
  // is saved, and it is deliberately not mirrored from the server, because the server keeps
  // only the digest of a staged image.
  const [preview, setPreview] = useState<string | null>(null)
  const [staged, setStaged] = useState<{ bytes: number; hash: string } | null>(null)
  const [busy, setBusy] = useState(false)

  const pick = async (f: File | undefined) => {
    setNote(null)
    if (!f) return
    if (!TYPES.includes(f.type)) {
      setNote('PNG, JPEG, GIF or WebP only — Discord refuses anything else.')
      return
    }
    if (f.size > MAX_BYTES) {
      setNote(`That file is ${(f.size / 1024 / 1024).toFixed(1)} MB. Keep it under ${MAX_BYTES / 1000} KB.`)
      return
    }
    const dataUri = await new Promise<string>((resolve, reject) => {
      const r = new FileReader()
      r.onload = () => resolve(String(r.result))
      r.onerror = () => reject(new Error('the browser could not read that file'))
      r.readAsDataURL(f)
    }).catch((e: unknown) => {
      setNote(e instanceof Error ? e.message : 'Could not read the file.')
      return null
    })
    if (!dataUri) return
    setBusy(true)
    try {
      const res = await stageAvatar(id, dataUri)
      setPreview(dataUri)
      setStaged({ bytes: res.bytes, hash: res.hash })
      setNote(`${f.name} · ${(res.bytes / 1024).toFixed(0)} KB · staged as ${res.hash}. Press Apply to put it on Discord.`)
    } catch (e) {
      setNote(e instanceof ApiError ? e.message : 'The API did not take the image.')
    } finally {
      setBusy(false)
    }
  }

  const push = async () => {
    setBusy(true)
    try {
      await applyIdentity(id)
      setStaged(null)
      setNote('Queued. The bot applies it the next time it breathes.')
    } catch (e) {
      setNote(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  const forget = async () => {
    setBusy(true)
    try {
      await clearAvatar(id)
      setPreview(null)
      setStaged(null)
      setNote('Staged image dropped, and the bot will clear Discord’s copy on the next apply.')
    } catch (e) {
      setNote(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Bot profile"
        hint="For this server. Nobody else sees these values."
        actions={
          <Button size="sm" onClick={push} disabled={busy}>
            Apply to Discord
          </Button>
        }
      />

      <div className="grid gap-3 lg:grid-cols-[2fr_1fr]">
        <SettingsForm
          feature="identity"
          title="Name and bio"
          icon={BadgeCheck}
          fields={[
            { kind: 'text', key: 'nick', label: 'Nickname', hint: '1–32 characters. Empty clears it and the username shows again.', max: 32, placeholder: 'Aerona' },
            { kind: 'textarea', key: 'bio', label: 'Bio', hint: 'Discord shows this under the profile. Max 190 characters.', rows: 2, max: 190 },
            { kind: 'textarea', key: 'about', label: 'About, in the dashboard', hint: 'Not a Discord field: it appears on this server’s pages and in /help.', rows: 2, max: 300 },
          ]}
        />

        <Card>
          <CardHeader>
            <CardTitle className="text-base">Avatar</CardTitle>
            <CardDescription>Square, at least 128 px. GIF moves; Discord keeps that in this server only.</CardDescription>
          </CardHeader>
          <CardContent className="grid gap-3">
            <div className="flex items-center gap-3">
              <span className="flex size-14 items-center justify-center overflow-hidden rounded-xl border bg-muted text-xs text-muted-foreground">
                {preview || typeof cfg.avatar === 'string' ? (
                  <img src={(preview ?? cfg.avatar) as string} alt="" className="size-full object-cover" />
                ) : (
                  'none'
                )}
              </span>
              <div className="grid gap-1">
                <Label htmlFor="avatar" className="sr-only">
                  Avatar file
                </Label>
                <input id="avatar" ref={file} type="file" accept={TYPES.join(',')} className="hidden" onChange={(e) => void pick(e.target.files?.[0])} />
                <div className="flex gap-2">
                  <Button size="sm" variant="outline" onClick={() => file.current?.click()}>
                    <Upload className="size-4" /> Choose
                  </Button>
                  {preview || cfg.avatarHash ? (
                    <Button size="sm" variant="ghost" onClick={() => void forget()} disabled={busy}>
                      <CircleX className="size-4" /> Clear
                    </Button>
                  ) : null}
                </div>
              </div>
            </div>
            {staged ? (
              <p className="font-mono text-[11px] text-muted-foreground">
                staged · {staged.hash} · {(staged.bytes / 1024).toFixed(0)} KB
              </p>
            ) : cfg.avatarHash ? (
              <p className="font-mono text-[11px] text-muted-foreground">on Discord · {String(cfg.avatarHash)}</p>
            ) : null}
            <p className="text-xs text-muted-foreground">
              {status?.ok ? (
                <span className="inline-flex items-center gap-1 text-foreground">
                  <CircleCheck className="size-4" /> Applied {ago(Number(status.reportedAt ?? 0))}
                </span>
              ) : status?.error ? (
                <span className="inline-flex items-center gap-1">
                  <CircleX className="size-4" /> {String(status.error).slice(0, 90)}
                </span>
              ) : (
                'Never applied from here yet.'
              )}
            </p>
            {note ? <p className="text-sm text-muted-foreground">{note}</p> : null}
            <Badge variant="outline">queued through the outbox, never a direct call</Badge>
          </CardContent>
        </Card>
      </div>
    </div>
  )
}
