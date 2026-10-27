/**
 * The Aerona mark, in the two inks the panel has.
 *
 * `aerona-mark.png` is dark ink for the light theme and `aerona-mark-light.png` is white ink for
 * the dark one. The swap is Tailwind's `dark:` variant rather than a hook: the logo then changes
 * with the same class that changes every token, so there is no second source of truth about which
 * theme is showing - and a browser with no JavaScript still gets the right one, because the class
 * is on `<html>` before React exists (see `public/theme-boot.js`).
 *
 * Both files come from `artwork/aerona.png` via `python3 scripts/make-icons.py`, which also writes
 * the favicons and the manifest icons - one file of art, no hand-drawn second version of the logo.
 */
import { cn } from '@/lib/utils'

export function AeronaMark({
  className,
  size = 32,
}: {
  className?: string
  /** Square edge length in px; the art is a square, so there is no aspect to get wrong. */
  size?: number
}) {
  const common = {
    alt: '',
    'aria-hidden': true as const,
    width: size,
    height: size,
    style: { width: size, height: size },
  }
  return (
    <span
      className={cn('inline-flex shrink-0 items-center justify-center', className)}
      style={{ width: size, height: size }}
    >
      <img {...common} src="/aerona-mark.png" className="h-full w-full object-contain dark:hidden" />
      <img
        {...common}
        src="/aerona-mark-light.png"
        className="hidden h-full w-full object-contain dark:block"
      />
    </span>
  )
}

/** Mark and wordmark, for the two screens in front of the panel. */
export function Brand({ size = 32, className }: { size?: number; className?: string }) {
  return (
    <span className={cn('inline-flex items-center gap-2', className)}>
      <AeronaMark size={size} />
      <span className="font-semibold leading-none">Aerona</span>
    </span>
  )
}
