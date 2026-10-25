/**
 * The one way a settings document is edited.
 *
 * A page says which fields exist and what they are called; this renders them, keeps a draft,
 * and PATCHes the feature's document. That is the whole contract, and it is why adding a
 * setting to the product costs one entry in a list rather than a new form, a new validation
 * path and a new way to be wrong.
 */
import { useMemo, useState } from 'react'
import type { LucideIcon } from 'lucide-react'
import { motion } from 'framer-motion'
import { Check, Search } from 'lucide-react'

import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from '@/components/ui/card'
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from '@/components/ui/command'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover'
import { Switch } from '@/components/ui/switch'
import { Textarea } from '@/components/ui/textarea'

import { cn } from './utils'
import { useGuild } from './guild'
import type { Feature } from './api'
import type { PickerOption } from './api'

export type FieldDef =
  | { kind: 'switch'; key: string; label: string; hint?: string }
  | { kind: 'int'; key: string; label: string; hint?: string; min?: number; max?: number; suffix?: string; offValue?: number }
  | { kind: 'color'; key: string; label: string; hint?: string }
  | { kind: 'text'; key: string; label: string; hint?: string; placeholder?: string; max?: number }
  | { kind: 'textarea'; key: string; label: string; hint?: string; rows?: number; placeholder?: string; max?: number }
  | { kind: 'enum'; key: string; label: string; hint?: string; options: { value: string; label: string }[] }
  | { kind: 'lines'; key: string; label: string; hint?: string; placeholder?: string }
  | { kind: 'ids'; key: string; label: string; hint?: string; source: 'channels' | 'roles'; multiple?: boolean }

export function SettingsForm({
  feature,
  title,
  description,
  fields,
  actions,
  icon: Icon,
  tone = 'default',
}: {
  feature: Feature
  title: string
  description?: string
  fields: FieldDef[]
  actions?: React.ReactNode
  /**
   * The card's own glyph. A page of three config cards with no marks on them reads as one long
   * form; the icon is what makes "Detection" and "What the bot does" findable by eye.
   */
  icon?: LucideIcon
  /**
   * Colour as a *meaning*: `warn` is a card whose setting changes what happens to a member, and
   * it is tinted so it cannot be mistaken for a preference. Kept to three values on purpose — a
   * palette per card is how a settings page turns into a rainbow.
   */
  tone?: 'default' | 'warn' | 'calm'
}) {
  const { settings, patch } = useGuild()
  const doc = settings(feature)
  const [draft, setDraft] = useState<Record<string, unknown>>({})
  const [busy, setBusy] = useState(false)

  // The draft overlays the saved document; unset keys fall back to it, so a field never
  // shows a blank where the server has a value.
  const value = (key: string) => (key in draft ? draft[key] : doc[key])
  const dirty = Object.keys(draft).length > 0

  const set = (key: string, v: unknown) => setDraft((d) => ({ ...d, [key]: v }))
  const reset = () => setDraft({})

  const save = async () => {
    setBusy(true)
    const ok = await patch(feature, draft)
    setBusy(false)
    if (ok) reset()
  }

  return (
    <Card>
      <CardHeader className={cn('flex-row items-center gap-3', tone === 'warn' && 'bg-amber-500/5')}>
        {Icon ? (
          <span
            className={cn(
              'grid size-8 shrink-0 place-items-center rounded-lg border',
              tone === 'warn' && 'border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400',
              tone === 'calm' && 'border-emerald-500/40 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400',
              tone === 'default' && 'bg-muted/40 text-muted-foreground',
            )}
            aria-hidden
          >
            <Icon className="size-4" />
          </span>
        ) : null}
        <div className="grid gap-0.5">
          <CardTitle>{title}</CardTitle>
          {description ? <CardDescription>{description}</CardDescription> : null}
        </div>
      </CardHeader>
      <CardContent className="grid gap-5">
        {fields.map((f) => (
          <FieldRow key={f.key} def={f} value={value(f.key)} onChange={(v) => set(f.key, v)} />
        ))}
      </CardContent>
      <CardFooter className="flex-wrap gap-2 border-t pt-4">
        <Button onClick={save} disabled={!dirty || busy}>
          {busy ? 'Saving…' : 'Save'}
        </Button>
        <Button variant="ghost" onClick={reset} disabled={!dirty || busy}>
          Discard
        </Button>
        {dirty ? <Badge variant="secondary">unsaved</Badge> : null}
        <div className="ms-auto flex gap-2">{actions}</div>
      </CardFooter>
    </Card>
  )
}

function FieldRow({
  def,
  value,
  onChange,
}: {
  def: FieldDef
  value: unknown
  onChange: (v: unknown) => void
}) {
  if (def.kind === 'switch') {
    const on = Boolean(value ?? false)
    return (
      <div className="flex items-start justify-between gap-4">
        <div className="grid gap-0.5">
          <Label htmlFor={`${def.key}`} className="font-medium">
            {def.label}
          </Label>
          {def.hint ? <p className="text-sm text-muted-foreground">{def.hint}</p> : null}
        </div>
        <Switch
          id={def.key}
          checked={on}
          onCheckedChange={(next) => onChange(Boolean(next))}
          aria-label={def.label}
        />
      </div>
    )
  }

  if (def.kind === 'int') {
    const n = typeof value === 'number' ? value : Number(value ?? 0)
    return (
      <label className="grid gap-1.5">
        <span className="font-medium text-sm">{def.label}</span>
        {def.hint ? <span className="text-sm text-muted-foreground">{def.hint}</span> : null}
        <span className="flex items-center gap-2">
          <Input
            type="number"
            className="w-28"
            min={def.min}
            max={def.max}
            value={Number.isFinite(n) ? n : 0}
            onChange={(e) => {
              const raw = Number(e.target.value)
              const clamped = Number.isFinite(raw) ? raw : def.offValue ?? 0
              onChange(Math.round(clamped))
            }}
          />
          {def.suffix ? <span className="text-sm text-muted-foreground">{def.suffix}</span> : null}
        </span>
      </label>
    )
  }

  if (def.kind === 'color') {
    // The document holds a number because that is what Discord's API takes, so the swatch and the
    // hex field are two views of one integer rather than a second colour format to keep in sync.
    const n = typeof value === 'number' ? value : Number(value ?? 0)
    const hex = `#${(n >>> 0).toString(16).padStart(6, '0').slice(-6)}`
    return (
      <label className="grid gap-1.5">
        <span className="font-medium text-sm">{def.label}</span>
        {def.hint ? <span className="text-sm text-muted-foreground">{def.hint}</span> : null}
        <span className="flex items-center gap-2">
          <input
            type="color"
            aria-label={`${def.label} colour`}
            value={hex}
            onChange={(e) => onChange(parseInt(e.target.value.slice(1), 16) || 0)}
            className="size-9 shrink-0 cursor-pointer rounded-md border bg-transparent p-1"
          />
          <Input
            className="w-28 font-mono"
            maxLength={7}
            value={hex}
            onChange={(e) => {
              const raw = e.target.value.trim().replace(/^#/, '')
              if (/^[\da-fA-F]{1,6}$/.test(raw)) onChange(Number.parseInt(raw.padStart(6, '0'), 16))
            }}
          />
        </span>
      </label>
    )
  }

  if (def.kind === 'text') {
    return (
      <label className="grid gap-1.5">
        <span className="font-medium text-sm">{def.label}</span>
        {def.hint ? <span className="text-sm text-muted-foreground">{def.hint}</span> : null}
        <Input
          value={String(value ?? '')}
          maxLength={def.max}
          placeholder={def.placeholder}
          onChange={(e) => onChange(e.target.value)}
        />
      </label>
    )
  }

  if (def.kind === 'textarea') {
    return (
      <label className="grid gap-1.5">
        <span className="font-medium text-sm">{def.label}</span>
        {def.hint ? <span className="text-sm text-muted-foreground">{def.hint}</span> : null}
        <Textarea
          rows={def.rows ?? 3}
          maxLength={def.max}
          placeholder={def.placeholder}
          value={String(value ?? '')}
          onChange={(e) => onChange(e.target.value)}
        />
      </label>
    )
  }

  if (def.kind === 'enum') {
    const current = String(value ?? '')
    return (
      <div className="grid gap-1.5">
        <span className="font-medium text-sm">{def.label}</span>
        {def.hint ? <span className="text-sm text-muted-foreground">{def.hint}</span> : null}
        <div className="flex flex-wrap gap-1.5">
          {def.options.map((o) => (
            <Button
              key={o.value}
              type="button"
              size="sm"
              variant={current === o.value ? 'default' : 'outline'}
              onClick={() => onChange(o.value)}
            >
              {o.label}
            </Button>
          ))}
        </div>
      </div>
    )
  }

  if (def.kind === 'lines') {
    const list = Array.isArray(value) ? (value as string[]) : []
    return (
      <label className="grid gap-1.5">
        <span className="font-medium text-sm">{def.label}</span>
        {def.hint ? <span className="text-sm text-muted-foreground">{def.hint}</span> : null}
        <Textarea
          rows={Math.min(6, Math.max(2, list.length + 1))}
          placeholder={def.placeholder}
          defaultValue={list.join('\n')}
          key={list.join('|')}
          onBlur={(e) =>
            onChange(
              e.target.value
                .split('\n')
                .map((s) => s.trim())
                .filter(Boolean),
            )
          }
        />
      </label>
    )
  }

  return <IdPicker def={def} value={value} onChange={onChange} />
}

/** Channel and role ids, picked from what the worker last saw. Falls back to a plain id
 *  box when no worker has reported yet, because an install can be configured before the
 *  bot has ever connected. */
function IdPicker({
  def,
  value,
  onChange,
}: {
  def: Extract<FieldDef, { kind: 'ids' }>
  value: unknown
  onChange: (v: unknown) => void
}) {
  const { report } = useGuild()
  const options: PickerOption[] = useMemo(() => {
    const p = report?.picker
    if (!p) return []
    return (def.source === 'channels' ? p.channels : p.roles) ?? []
  }, [report, def.source])

  const ids: string[] = useMemo(() => {
    if (def.multiple) return Array.isArray(value) ? (value as string[]).map(String) : []
    return value ? [String(value)] : []
  }, [value, def.multiple])

  const name = (id: string) => options.find((o) => o.id === id)?.name ?? id
  const [open, setOpen] = useState(false)

  const toggle = (id: string) => {
    if (!def.multiple) {
      onChange(id)
      setOpen(false)
      return
    }
    onChange(ids.includes(id) ? ids.filter((x) => x !== id) : [...ids, id])
  }

  return (
    <div className="grid gap-1.5">
      <span className="font-medium text-sm">{def.label}</span>
      {def.hint ? <span className="text-sm text-muted-foreground">{def.hint}</span> : null}
      <div className="flex flex-wrap items-center gap-1.5">
        <Popover open={open} onOpenChange={setOpen}>
          <PopoverTrigger>
            <Button type="button" variant="outline" size="sm">
              <Search /> {def.source === 'channels' ? 'Pick a channel' : 'Pick a role'}
            </Button>
          </PopoverTrigger>
          <PopoverContent align="start" className="w-72 p-0">
            <Command>
              <CommandInput placeholder="Search…" />
              <CommandList>
                <CommandEmpty>
                  {options.length === 0 ? (
                    <span className="text-sm">Nothing reported by the bot yet. Type the id instead.</span>
                  ) : (
                    'No match.'
                  )}
                </CommandEmpty>
                <CommandGroup>
                  {options.map((o) => (
                    <CommandItem key={o.id} value={`${o.name} ${o.id}`} onSelect={() => toggle(o.id)}>
                      <span className="flex-1 truncate">{o.name}</span>
                      {ids.includes(o.id) ? <Check className="size-4" /> : null}
                    </CommandItem>
                  ))}
                </CommandGroup>
              </CommandList>
            </Command>
          </PopoverContent>
        </Popover>
        {ids.map((id) => (
          <motion.span key={id} initial={{ opacity: 0, scale: 0.96 }} animate={{ opacity: 1, scale: 1 }}>
            <Badge variant="secondary" className="gap-1 py-1 pe-1">
              {name(id)}
              <button
                type="button"
                className="rounded-sm px-0.5 text-muted-foreground hover:text-foreground"
                onClick={() => onChange(def.multiple ? ids.filter((x) => x !== id) : '')}
                aria-label={`Remove ${name(id)}`}
              >
                ×
              </button>
            </Badge>
          </motion.span>
        ))}
        <Input
          className="w-40"
          placeholder="or paste an id"
          value=""
          onChange={(e) => {
            const v = e.target.value.trim()
            if (/^\d{15,25}$/.test(v)) onChange(def.multiple ? [...ids, v] : v)
          }}
        />
      </div>
    </div>
  )
}
