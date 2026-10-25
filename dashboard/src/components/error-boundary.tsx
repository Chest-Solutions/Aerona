/**
 * The last thing standing between a crashing component and a blank panel.
 *
 * This exists because of a specific failure that cost two rounds of diagnosis: a vendored
 * `components/ui` wrapper mapped a menu caption onto Base UI's `Menu.GroupLabel`, that primitive
 * reads a context that only a `Menu.Group` provides, and Base UI *throws* rather than degrading.
 * One throw during render unmounted the whole page, so the operator saw "the tabs don't work" and
 * a single console line — and the minified build said only `Base UI error #31`. A self-hosted panel
 * must not hide that kind of fault in the console, because the person reading it is the person who
 * has to decide whether to file it, so the message goes on the page.
 *
 * Deliberately built from nothing but JSX and utility classes: the event being handled is a
 * component crashing, and a card assembled from `components/ui` would be a white screen with
 * better intentions. `main.tsx` wraps the whole tree in this, not just the page region, because the
 * crash that started all this came from the sidebar.
 */
import { Component, type ErrorInfo, type ReactNode } from 'react'

type Props = { children: ReactNode }
type State = { error: Error | null }

export class ErrorBoundary extends Component<Props, State> {
  override state: State = { error: null }

  static getDerivedStateFromError(error: Error): State {
    return { error }
  }

  override componentDidCatch(error: Error, info: ErrorInfo) {
    // Both the message and the component stack, once, in the log the operator is already told to
    // open: `#31` was decodable only because the browser had kept the stack.
    console.error('[aerona] a page failed to render\n', error, '\n', info.componentStack)
  }

  override render() {
    const { error } = this.state
    if (!error) return this.props.children
    return (
      <main className="flex min-h-svh items-center justify-center bg-background p-6 text-foreground">
        <div className="w-full max-w-lg rounded-lg border border-border bg-card p-6">
          <p className="font-mono text-xs uppercase tracking-wide text-muted-foreground">
            This page failed to render
          </p>
          <h1 className="mt-2 text-lg font-medium">Aerona is still running — only this view broke</h1>
          <p className="mt-2 text-sm text-muted-foreground">
            Nothing was sent to Discord and no setting was changed by this. The text below is what to
            paste when reporting it.
          </p>
          <pre className="mt-4 max-h-40 overflow-auto rounded-md bg-muted p-3 font-mono text-xs whitespace-pre-wrap">
            {error.message || String(error)}
          </pre>
          {error.stack ? (
            <details className="mt-3">
              <summary className="cursor-pointer font-mono text-xs text-muted-foreground">stack</summary>
              <pre className="mt-2 max-h-64 overflow-auto font-mono text-[11px] whitespace-pre-wrap opacity-80">
                {error.stack}
              </pre>
            </details>
          ) : null}
          <div className="mt-5 flex gap-2">
            <button
              type="button"
              className="rounded-md border border-border px-3 py-1.5 text-sm hover:bg-accent"
              onClick={() => this.setState({ error: null })}
            >
              Try this page again
            </button>
            <a
              href="/"
              className="rounded-md bg-foreground px-3 py-1.5 text-sm text-background hover:opacity-90"
            >
              Back to the server list
            </a>
          </div>
        </div>
      </main>
    )
  }
}
