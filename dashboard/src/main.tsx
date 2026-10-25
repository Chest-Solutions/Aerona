import { StrictMode } from 'react'
import { hydrateRoot, createRoot } from 'react-dom/client'

import { App } from './App'
import { ErrorBoundary } from './components/error-boundary'
import './index.css'

const container = document.getElementById('root')
if (!container) throw new Error('missing #root')

/**
 * The prerendered markup is real content, so hydrate it — but inside a transition, so
 * a slow hydration of a large table never blocks the first paint the browser already
 * made. If the markup was the empty placeholder (dev server, or a route the
 * prerenderer does not know), mount fresh instead.
 */
// The prerenderer always emits a shell with `#main`, so that is the marker.
const prerendered = container.querySelector('#main') !== null
// The boundary is outside the router on purpose: the fault it catches can come from the shell
// itself (the guild sidebar's menu did), and a boundary that only wraps `<main>` would let that
// blank the page it is standing on top of.
const tree = (
  <StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </StrictMode>
)
if (prerendered) {
  hydrateRoot(container, tree, { onRecoverableError: (e) => console.warn('hydration recovered', e) })
} else {
  container.innerHTML = ''
  createRoot(container).render(tree)
}
