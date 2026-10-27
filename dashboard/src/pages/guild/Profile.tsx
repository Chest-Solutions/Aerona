/**
 * The bot's face: what it is called here, and what it looks like everywhere.
 *
 * A nickname is a fact about this server's member, so it changes for nobody else. The avatar, the
 * bio, the banner and the two decorations are facts about the *account* - Discord has no per-guild
 * banner and no per-guild decoration - so setting one of those here sets it in every server the bot
 * sits in, and the card that does it says so on its face.
 *
 * An image travels as a data URI into a staged report and never into a settings document: that
 * document is copied whole into `config_history` on every save, and a megabyte of base64 in each of
 * those rows would bury the audit. Nothing reaches Discord until Apply, which the worker runs out of
 * the outbox, and whatever Discord refuses comes back as the report under the buttons rather than
 * being quietly assumed to have worked.
 */
import { useRef, useState } from 'react'
import { BadgeCheck, CircleCheck, CircleX, Upload } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'

import { ApiError, applyIdentity, clearAvatar, clearBanner, setDecoration, stageAvatar, stageBanner } from '../../lib/api'
import { useGuild } from '../../lib/guild'
import { ago, PageHeader } from './shared'
import { FieldHint, SettingsForm } from '../../lib/settings-form'

const TYPES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp']
const MAX_BYTES = 900_000

/** The two decorations, as one list, so a row cannot be added to one and forgotten on the other.
 *  Discord publishes no catalogue a bot may read, so the panel offers what the worker can see -
 *  the decorations members of this server are wearing - plus a field for an id typed in. */
const DECORATIONS: {
  key: 'avatar_decoration_id' | 'banner_decoration_id'
  label: string
  hint: string
}[] = [
  {
    key: 'avatar_decoration_id',
    label: 'Avatar decoration',
    hint: 'The ring around the avatar. It belongs to the account, so it shows in every server, and Discord only accepts one the account owns.',
  },
  {
    key: 'banner_decoration_id',
    label: 'Banner decoration',
    hint: 'The pattern behind the profile banner. Account-wide, and owned by the account - an id it does not have is refused.',
  },
]

/** The picture of a decoration, by its asset name. A courtesy in the picker: the *id* is what gets
 *  saved, so an asset that will not load here still works in Discord. */
function decoUrl(asset: string): string {
  const ext = asset.startsWith('a_') ? 'gif' : 'png'
  return `https://cdn.discordapp.com/avatar-decoration-presets/${asset}.${ext}`
}

/** An image the operator picked, staged on the server, shown here until the worker puts it on
 *  Discord. Avatar and banner are the same shape - a picture, a digest, two buttons - so they are
 *  the same component, and neither card can drift from the other.
 */
function ImageCard({
  title,
  note,
  src,
  hash,
  busy,
  round,
  onPick,
  onClear,
}: {
  title: string
  note: string
  src: string | null
  hash: unknown
  busy: boolean
  round?: boolean
  onPick: (f: File | undefined) => void
  onClear: () => void
}) {
  const file = useRef<HTMLInputElement>(null)
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{title}</CardTitle>
        <CardDescription>{note}</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-3">
        <div className="flex items-center gap-3">
          <span
            className={`flex size-14 shrink-0 items-center justify-center overflow-hidden border bg-muted text-xs text-muted-foreground ${
              round ? 'rounded-full' : 'rounded-xl'
            }`}
          >
            {src ? <img src={src} alt="" className="size-full object-cover" /> : 'none'}
          </span>
          <div className="grid gap-1">
            <Label htmlFor={`file-${title}`} className="sr-only">
              {title} file
            </Label>
            <input
              id={`file-${title}`}
              ref={file}
              type="file"
              accept={TYPES.join(',')}
              className="hidden"
              onChange={(e) => onPick(e.target.files?.[0])}
            />
            <div className="flex gap-2">
              <Button size="sm" variant="outline" onClick={() => file.current?.click()} disabled={busy}>
                <Upload className="size-4" /> Choose
              </Button>
              {src || hash ? (
                <Button size="sm" variant="ghost" onClick={onClear} disabled={busy}>
                  <CircleX className="size-4" /> Clear
                </Button>
              ) : null}
            </div>
          </div>
        </div>
      </CardContent>
    </Card>
  )
}

export default function Profile() {
  const { id, settings, report, reload } = useGuild()
  const cfg = settings('identity') as Record<string, unknown>
  const status = report?.identity ?? null
  const decorations = report?.picker?.decorations ?? []
  const [note, setNote] = useState<string | null>(null)
  // The data URIs live here and nowhere else: they are what the two previews show before anything
  // is saved, and they are deliberately not mirrored from the server, because the server keeps
  // only the digest of a staged image.
  const [preview, setPreview] = useState<{ avatar: string | null; banner: string | null }>({
    avatar: null,
    banner: null,
  })
  const [staged, setStaged] = useState({ avatar: false, banner: false })
  /** What an operator has typed for a decoration and not yet asked for: kept apart from the saved
   *  id, so a half-typed id stays visible and unsent. */
  const [draft, setDraft] = useState<Record<string, string>>({})
  const [busy, setBusy] = useState(false)

  /** A settings document holds `null` for "nobody has said", `""` for "take it off", and an id for
   *  "wear this" - three states, so the page reads it once instead of guessing twice. */
  const saved = (key: string): string => (typeof cfg[key] === 'string' ? (cfg[key] as string) : '')

  const pick = async (slot: 'avatar' | 'banner', f: File | undefined) => {
    setNote(null)
    if (!f) return
    if (!TYPES.includes(f.type)) {
      setNote('PNG, JPEG, GIF or WebP only - Discord refuses anything else.')
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
      const res = slot === 'avatar' ? await stageAvatar(id, dataUri) : await stageBanner(id, dataUri)
      setPreview((p) => ({ ...p, [slot]: dataUri }))
      setStaged((p) => ({ ...p, [slot]: true }))
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
      setStaged({ avatar: false, banner: false })
      setPreview({ avatar: null, banner: null })
      setNote('Queued. The bot applies it the next time it breathes.')
    } catch (e) {
      setNote(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  const forget = async (slot: 'avatar' | 'banner') => {
    setBusy(true)
    try {
      if (slot === 'avatar') await clearAvatar(id)
      else await clearBanner(id)
      setPreview((p) => ({ ...p, [slot]: null }))
      setStaged((p) => ({ ...p, [slot]: false }))
      setNote('Staged image dropped, and the bot will clear Discord’s copy on the next apply.')
    } catch (e) {
      setNote(e instanceof ApiError ? e.message : 'The API did not answer.')
    } finally {
      setBusy(false)
    }
  }

  /** Ask Discord for a decoration by id, or take it off with the empty string. Whether the account
   *  owns that decoration is Discord’s to decide, and its answer lands in the report under the
   *  cards - which is why this says "asked for" and never "set". */
  const wear = async (key: (typeof DECORATIONS)[number]['key'], value: string) => {
    const body =
      key === 'avatar_decoration_id' ? { avatar_decoration_id: value } : { banner_decoration_id: value }
    setBusy(true)
    try {
      await setDecoration(id, body)
      setDraft((d) => ({ ...d, [key]: '' }))
      setNote(
        value
          ? 'Asked for it. If the account does not own that decoration, the line under the cards says Discord said no.'
          : 'Taken off, and queued for the next apply.',
      )
      await reload()
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
            {
              kind: 'text',
              key: 'nick',
              label: 'Nickname',
              hint: '2–32 characters. Clear it and the username shows again.',
              max: 32,
              placeholder: 'Aerona',
            },
            { kind: 'textarea', key: 'bio', label: 'Bio', rows: 2, max: 190 },
            {
              kind: 'textarea',
              key: 'about',
              label: 'About, in the dashboard',
              hint: 'Not a Discord field: it appears on this server’s pages and in /help.',
              rows: 2,
              max: 300,
            },
          ]}
        />

        <div className="grid gap-3">
          <ImageCard
            title="Avatar"
            note="Square, at least 128 px. Account-wide, not only here."
            src={preview.avatar ?? (typeof cfg.avatar === 'string' ? (cfg.avatar as string) : null)}
            hash={cfg.avatar_hash}
            busy={busy}
            round
            onPick={(f) => void pick('avatar', f)}
            onClear={() => void forget('avatar')}
          />
          <ImageCard
            title="Banner"
            note="Wide, 960x360 or near it. The account's own banner, so every server sees it - and a bot may write it only if Discord lets the account keep its profile."
            src={preview.banner}
            hash={cfg.banner_hash}
            busy={busy}
            onPick={(f) => void pick('banner', f)}
            onClear={() => void forget('banner')}
          />
          <HashLine
            staged={staged}
            rows={[
              ['avatar', cfg.avatar_hash],
              ['banner', cfg.banner_hash],
            ]}
          />
          <p className="text-xs text-muted-foreground">
            {status?.ok ? (
              <span className="inline-flex flex-wrap items-center gap-1 text-foreground">
                <CircleCheck className="size-4" /> Applied {ago(Number(status.reportedAt ?? 0))}
                <span className="text-muted-foreground">
                  · this server {status.member ? 'took it' : 'had nothing to take'} · the account{' '}
                  {status.profile ? 'took it' : 'refused or had nothing'}
                </span>
              </span>
            ) : status?.error ? (
              <span className="inline-flex items-start gap-1">
                <CircleX className="size-4" /> {String(status.error).slice(0, 240)}
              </span>
            ) : (
              'Never applied from here yet.'
            )}
          </p>
          {status?.refused && Object.keys(status.refused).length ? (
            <ul className="grid gap-1 text-xs text-muted-foreground">
              {Object.entries(status.refused).map(([field, why]) => (
                <li key={field}>
                  <span className="font-mono">{field}</span> - {String(why)}
                </li>
              ))}
            </ul>
          ) : null}
          {note ? <p className="text-sm text-muted-foreground">{note}</p> : null}

          <Card>
            <CardHeader>
              <CardTitle className="text-base">Decorations</CardTitle>
              <CardDescription>
                The Nitro extras, if the account owns them. Chosen from what members of this server wear, or
                typed in.
              </CardDescription>
            </CardHeader>
            <CardContent className="grid gap-4">
              {DECORATIONS.map((d) => {
                const current = saved(d.key)
                const typed = draft[d.key] ?? ''
                return (
                  <div key={d.key} className="grid gap-2">
                    <span className="flex items-center gap-1.5 text-sm font-medium">
                      {d.label}
                      <FieldHint text={d.hint} />
                    </span>
                    {decorations.length ? (
                      <div className="flex flex-wrap items-center gap-1.5">
                        {decorations.map((x) => {
                          const worn = current === x.id
                          return (
                            <button
                              key={x.id}
                              type="button"
                              title={`${x.id} - worn by ${x.usedBy} member(s) here`}
                              aria-pressed={worn}
                              onClick={() => void wear(d.key, worn ? '' : x.id)}
                              className={`grid gap-0.5 rounded-lg border p-1.5 text-center transition-colors ${
                                worn ? 'border-primary bg-primary/10' : 'hover:bg-muted'
                              }`}
                            >
                              <img src={decoUrl(x.asset)} alt="" className="size-8 object-contain" />
                              <span className="font-mono text-[10px] text-muted-foreground">{x.usedBy}</span>
                            </button>
                          )
                        })}
                      </div>
                    ) : (
                      <p className="text-xs text-muted-foreground">
                        Nothing reported yet - the bot can only list the decorations members here are wearing, so
                        type an id, or refresh the picker from a page that has one.
                      </p>
                    )}
                    <div className="flex flex-wrap items-center gap-1.5">
                      <Input
                        value={typed || current}
                        onChange={(e) => setDraft((p) => ({ ...p, [d.key]: e.target.value }))}
                        placeholder="or a decoration id"
                        maxLength={20}
                        className="w-52 font-mono text-xs"
                        aria-label={`${d.label} id`}
                      />
                      <Button
                        size="sm"
                        variant="outline"
                        disabled={busy || !typed.trim() || typed.trim() === current}
                        onClick={() => void wear(d.key, typed.trim())}
                      >
                        Wear it
                      </Button>
                      {current ? (
                        <Button size="sm" variant="ghost" disabled={busy} onClick={() => void wear(d.key, '')}>
                          <CircleX className="size-4" /> Take it off
                        </Button>
                      ) : null}
                    </div>
                    <p className="font-mono text-[11px] text-muted-foreground">
                      {current ? `saved · ${current}` : 'not set · Discord decides'}
                    </p>
                  </div>
                )
              })}
            </CardContent>
          </Card>
        </div>
      </div>
    </div>
  )
}

/** The one line that says whether a picture is waiting to be sent or already sent. Both images share
 *  it because the distinction - a digest on disk is not the same as bytes queued for Discord - is the
 *  one thing an operator most often reads wrong here.
 */
function HashLine({
  staged,
  rows,
}: {
  staged: { avatar: boolean; banner: boolean }
  rows: [string, unknown][]
}) {
  const named = () => rows.filter(([, hash]) => hash).map(([slot, hash]) => `${slot} ${String(hash)}`)
  const anyStaged = staged.avatar || staged.banner
  const text = named()
  if (!text.length) return null
  return (
    <p className="font-mono text-[11px] text-muted-foreground">
      {anyStaged ? 'staged, press Apply · ' : 'on Discord · '}
      {text.join(' · ')}
    </p>
  )
}
