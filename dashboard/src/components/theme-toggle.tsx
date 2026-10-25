/**
 * Light, dark, system — three options, one class on `<html>`.
 *
 * This is not the panel's old theme switch: the stylesheet is shadcn's own, so `.dark` is
 * the only thing it understands, and the values behind it belong to the registry. The
 * bootstrap snippet exported below is inlined in `index.html` so a browser that saved
 * "dark" does not paint light for one frame while React is still loading.
 */
import { createContext, useCallback, useContext, useEffect, useState } from 'react'
import { Monitor, Moon, Sun } from 'lucide-react'

import { Button } from '@/components/ui/button'

export type Theme = 'light' | 'dark' | 'system'
const KEY = 'aerona.theme'

const resolve = (t: Theme): 'light' | 'dark' =>
  t === 'system'
    ? window.matchMedia?.('(prefers-color-scheme: dark)').matches
      ? 'dark'
      : 'light'
    : t

export const BOOTSTRAP = `(function(){try{var t=localStorage.getItem('${KEY}')||'system';var d=t==='dark'||(t==='system'&&window.matchMedia&&matchMedia('(prefers-color-scheme: dark)').matches);document.documentElement.classList.toggle('dark',d);}catch(e){}})()`

const Ctx = createContext<{ theme: Theme; set: (t: Theme) => void }>({ theme: 'system', set: () => {} })

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const [theme, setTheme] = useState<Theme>('system')

  useEffect(() => {
    const saved = (localStorage.getItem(KEY) as Theme | null) ?? 'system'
    setTheme(saved)
    const paint = () => document.documentElement.classList.toggle('dark', resolve(saved) === 'dark')
    paint()
    if (saved !== 'system') return
    const mq = window.matchMedia('(prefers-color-scheme: dark)')
    mq.addEventListener('change', paint)
    return () => mq.removeEventListener('change', paint)
  }, [])

  const set = useCallback((t: Theme) => {
    localStorage.setItem(KEY, t)
    setTheme(t)
    document.documentElement.classList.toggle('dark', resolve(t) === 'dark')
  }, [])

  return <Ctx.Provider value={{ theme, set }}>{children}</Ctx.Provider>
}

export function ThemeToggle() {
  const { theme, set } = useContext(Ctx)
  const next: Theme = theme === 'light' ? 'dark' : theme === 'dark' ? 'system' : 'light'
  const Icon = theme === 'light' ? Sun : theme === 'dark' ? Moon : Monitor
  return (
    <Button variant="outline" size="sm" onClick={() => set(next)} aria-label={`Theme: ${theme}`}>
      <Icon className="size-4" />
      <span className="hidden sm:inline capitalize">{theme}</span>
    </Button>
  )
}

/** The resolved preference, for anything that must follow it — sonner's toaster is the only
 *  consumer today, and it is why the provider exists rather than a bare class toggle. */
export function useTheme() {
  return useContext(Ctx)
}
