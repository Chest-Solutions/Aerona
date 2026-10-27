/**
 * Build and run `overlay-fixture.tsx`: every popup component in the panel, rendered once, so a
 * part that reads a missing context (`Base UI error #31` in a production build) fails `npm run
 * build` instead of blanking somebody's moderation page.
 *
 * It is a *render*, not a grep, which is the point: the vendored registry files are plain TSX that
 * type-checks happily while throwing at runtime, and `renderToStaticMarkup` runs the component
 * bodies - the same function a browser would call - without needing a DOM. Where the server cannot
 * reach (a portal that waits for `document`), the fixture says so out loud rather than counting the
 * case as a pass, so the run never quietly shrinks to nothing.
 */
import { build } from 'vite'
import path from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { mkdirSync } from 'node:fs'

const here = path.dirname(fileURLToPath(import.meta.url))
const root = path.resolve(here, '..')
// Under `node_modules/.cache`, and nowhere else. The bundle keeps `react-dom` external - that is how
// vite's SSR builds work - so it has to be placed where `node_modules` resolves, and `/tmp` is not:
// the first version wrote there and node answered `Cannot find package 'react-dom'`. `node_modules` is
// excluded from the release archive by rule and is disposable by `npm ci`, so an audit cannot change
// what a release contains, which is the property that matters here.
const out = path.join(root, 'node_modules/.cache/aerona-overlay-audit')
mkdirSync(out, { recursive: true })

await build({
  root,
  logLevel: 'warn',
  // The fixture is a script, not a shipped page: the project config supplies the aliases the vendored
  // components import (`cn`, `@/…`), and `build.ssr` is how `npm run build:ssr` names its entry, so
  // this is the same path the prerenderer takes - with its own output directory.
  build: {
    ssr: path.join(here, 'overlay-fixture.tsx'),
    outDir: out,
    emptyOutDir: true,
    minify: false,
    copyPublicDir: false,
  },
})

const { run } = await import(pathToFileURL(path.join(out, 'overlay-fixture.js')).href)
const { failures, asserted, unasserted } = run()

const lines = []
lines.push(`  overlays rendered: ${asserted} asserted, ${unasserted.length} popup cases compiled`)
for (const f of failures) lines.push(`  ✗ ${f}`)
console.log(lines.join('\n'))
if (failures.length) {
  console.error('\n  a vendored component threw or was not reached; a page that renders it would unmount\n')
  process.exit(1)
}
if (asserted === 0) {
  console.error('\n  nothing was asserted - the fixture proves nothing; fix it rather than shipping the gate\n')
  process.exit(1)
}
// Deliberately a note, not a pass: a portal's contents need effects, which a server render does not
// run, so those cases are covered by the static caption rule in `prerender.mjs` and by a browser.
console.log(`  overlay audit clean (popup bodies of ${unasserted.join(', ')} are out of SSR's reach)`)
