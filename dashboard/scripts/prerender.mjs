/**
 * Static pass over every route.
 *
 * For each path in the router table this renders the app to HTML, writes it to
 * `dist/<path>/index.html` (so `ServeDir`'s index resolution finds it without rewrite
 * rules) and injects the page's own head tags. It then audits what it produced and
 * exits non-zero on a broken internal link, a dead anchor, a missing breadcrumb or
 * invalid JSON-LD: a claim of a clean SEO run has to be checked by the build, not by
 * prose.
 */
import { readFile, writeFile, mkdir } from 'node:fs/promises'
import { existsSync, readdirSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const root = path.resolve(here, '..')
const dist = path.join(root, 'dist')
const ssrEntry = path.join(root, 'dist-server', 'entry-server.js')

const origin = process.env.AERONA_SITE_ORIGIN || 'https://aerona.local'
const problems = []
const fail = (msg) => problems.push(msg)

if (!existsSync(ssrEntry)) {
  console.error('✗ dist-server/entry-server.js is missing — run `npm run build:ssr` first.')
  process.exit(1)
}

const { render, paths } = await import(pathToFileURL(ssrEntry).href)
if (!Array.isArray(paths) || paths.length === 0) {
  console.error('✗ entry-server.js did not export a route table')
  process.exit(1)
}

// The template has to survive being written over: the root route IS `dist/index.html`.
// A pristine build still carries the slot comments, so the cache is refreshed whenever
// it is, and only reused otherwise — which is what makes `npm run prerender` on its own
// work without ever serving a stale bundle. It lives *outside* `dist`, because anything
// inside the document root is a URL: a build artefact nobody meant to publish.
const cachePath = path.join(root, '.prerender-template.html')
const built = path.join(dist, 'index.html')
let template = ''
if (existsSync(built)) {
  const fresh = await readFile(built, 'utf8')
  if (fresh.includes('<!--app-html-->') && fresh.includes('<!--head-->')) {
    template = fresh
    await writeFile(cachePath, fresh, 'utf8')
  }
}
if (!template) {
  if (!existsSync(cachePath)) {
    console.error('✗ dist/index.html has no slot comments and no cached template exists — run `npm run build` first.')
    process.exit(1)
  }
  template = await readFile(cachePath, 'utf8')
}
if (!template.includes('<!--app-html-->')) fail('the template has no <!--app-html--> slot (rebuild the client bundle)')
if (!template.includes('<!--head-->')) fail('the template has no <!--head--> slot (rebuild the client bundle)')

const bootPath = path.join(dist, 'theme-boot.js')
if (!existsSync(bootPath)) {
  fail('dashboard/public/theme-boot.js was not built into dist/ — the theme would be decided by React, one paint late')
}
const bootBody = await readFile(bootPath, 'utf8')

const pages = new Map()
const rows = []
const pagesMetaIndexability = new Map()

for (const p of paths) {
  const { html, head } = await render(p, origin)
  const file = p === '/' ? path.join(dist, 'index.html') : path.join(dist, p.replace(/^\/+/, ''), 'index.html')
  // A replacement function, so a `$&` inside a rendered page is never reinterpreted.
  // The template carries a default title and description so the dev server and a
  // no-JS first byte still say something; a prerendered page must not ship two, so the
  // generic pair is dropped in favour of the page's own.
  const filled = template
    .replace(/<title>[\s\S]*?<\/title>\s*/, '')
    .replace(/<meta name="description"[^>]*>\s*/, '')
    .replace('<!--app-html-->', () => html)
    .replace('<!--head-->', () => head || '<meta name="robots" content="noindex" /><!-- head builder unavailable -->')
  const indexable = !head.includes('noindex')
  // An OG image that 404s is the most common silent SEO failure: the tag is written
  // into every page, and nobody notices until a share looks empty.
  for (const m of filled.matchAll(/property="og:image" content="([^"]+)"/g)) {
    const file = new URL(m[1]).pathname
    if (!existsSync(path.join(dist, file))) fail(`${p}: og:image ${m[1]} was not built`)
  }
  const titles = (filled.match(/<title>/g) ?? []).length
  if (titles !== 1) fail(`${p}: ${titles} <title> tags in the output`)
  if (!filled.includes('<meta name="viewport"')) fail(`${p}: no viewport meta`)
  // The theme is one class on <html>, set before the stylesheet paints, by a *file*: the
  // response's Content-Security-Policy is `script-src 'self'` with no hashes and no
  // unsafe-inline, so an inline block here is not a style choice — the browser refuses it, the
  // class never lands, and React hydrates the wrong colours and logs a recoverable mismatch
  // nobody can act on. Three halves of that promise get checked: the link is first, the file
  // was built, and nothing in <head> is inline for the policy to refuse.
  const headHtml = filled.slice(filled.indexOf('<head>'), filled.indexOf('</head>'))
  if (!/<script src="\/theme-boot\.js"><\/script>/.test(headHtml)) {
    fail(`${p}: no <script src="/theme-boot.js"> in <head>, so the theme is decided after first paint`)
  }
  if (headHtml.indexOf('<script src="/theme-boot.js">') > headHtml.indexOf('<link rel="stylesheet"')) {
    fail(`${p}: the theme bootstrap is linked after the stylesheet, which is the flash it exists to remove`)
  }
  if (!bootBody.includes('aerona.theme')) fail('public/theme-boot.js no longer reads aerona.theme from localStorage')
  // Only an *executable* inline script is the CSP's business: the JSON-LD the head builder puts
  // in the same slot is a data block, never run, and refusing it here would teach the next person
  // to delete structured data instead of fixing the rule.
  const inlineExecutable = [...headHtml.matchAll(/<script([^>]*)>/g)].filter((m) => {
    if (/\bsrc=/.test(m[1])) return false
    const type = /\btype="([^"]*)"/.exec(m[1])?.[1]
    return !type || type === 'module' || type === 'text/javascript' || type === 'application/javascript'
  })
  if (inlineExecutable.length) fail(`${p}: an inline executable <script> in <head> — CSP script-src 'self' refuses it`);
  if (/<html[^>]*\bclass="[^"]*\bdark\b/.test(filled)) fail(`${p}: <html> is pinned to the dark class, so a light-mode reader gets a dark flash`)
  if (!filled.includes('name="theme-color"')) fail(`${p}: no theme-color meta, so the browser chrome paints the wrong colour`)
  if (!filled.includes('name="color-scheme"')) fail(`${p}: no color-scheme meta, so scrollbars and form controls stay light`)
  // One <h1> is a rule, not a preference: a second is usually a decorative title someone
  // promoted, and it splits the page in two for an assistive reader.
  const h1s = (filled.match(/<h1[\s>]/g) ?? []).length
  if (indexable && h1s > 1 && p !== '/404/') fail(`${p}: ${h1s} <h1> tags, expected exactly one`)
  if (indexable && !filled.includes('property="og:image:alt"')) fail(`${p}: the share image has no alt text`)
  if (indexable && !/<title>[^<]{12,70}<\/title>/.test(filled)) fail(`${p}: <title> outside the 12-70 character band`)
  await mkdir(path.dirname(file), { recursive: true })
  await writeFile(file, filled, 'utf8')
  pages.set(p, filled)
  pagesMetaIndexability.set(p, indexable)
  // Indexability is read from the head that was just generated, not from a list of
  // paths: the page decides, so `noindex` cannot drift out of sync with the routes.
  rows.push({ path: p, bytes: Buffer.byteLength(filled), h1: /<h1[\s>]/.test(filled), indexable })
}

// --------------------------------------------------------------------- shell
// `/guild/123/tickets/` has no file and never will — the server id is in the URL — so the API
// serves this document for that prefix. It is the *template*, slots and all: an empty `#root`,
// so the client mounts rather than hydrating, and none of the root page's markup for React to
// find missing (a prerendered `/` under a guild URL is a hydration mismatch, logged as a
// recovered error on every deep link). The `<!--app-html-->` comment is markup-neutral, so
// leaving the slots unfilled is what makes the file a shell instead of a stale page.
const shell = template
  // The head slot is left empty rather than filled with a second `noindex`: the template already
  // carries the robots meta and the viewport, and duplicating them here would suggest the shell is
  // a different kind of document when it is the same one with its slots unfilled.
  .replace('<!--head-->', () => '')
  .replace(/<title>[\s\S]*?<\/title>\s*/, '<title>Aerona dashboard</title>\n')
await writeFile(path.join(dist, 'shell.html'), shell, 'utf8')
if (shell.includes('id="main"')) fail('shell.html carries prerendered markup, so a deep link hydrates against the wrong page')
if (!/<script src="\/theme-boot\.js"><\/script>/.test(shell)) fail('shell.html lost the theme bootstrap')
// Not a route — nobody links to it, and counting it as one would inflate the number the
// summary prints for the sitemap's sake.
const shellBytes = Buffer.byteLength(shell)

// ------------------------------------------------------------ vendored-component audit
// A Base UI part reads the context of the ancestor it belongs to and *throws* when it is used bare.
// Minified that arrives as `Base UI error #31`, and in production a throw during render unmounts the
// page — so what the operator sees is "the server pages don't work", with one console line as the
// only evidence. This repo shipped that bug for a round: `DropdownMenuLabel` was `Menu.GroupLabel`
// underneath, and a caption at the top of a menu has no `Menu.Group`, so opening the guild switcher
// took the guild page with it. `node_modules/@base-ui/react` carries 58 "…Context is missing"
// messages (`grep -rhoE "[A-Za-z]+Context is missing" node_modules/@base-ui/react`) — the class is
// not hypothetical, it is what a registry component can hand you, and no type check sees it.
//
// The rule is the narrow, false-positive-free half of the lesson: an export named `…Label` is a
// caption, and a caption must not be a group-scoped primitive — those are `aria-hidden` on top of
// requiring an ancestor, because their job is to be a group's `aria-labelledby` target. A
// group-scoped label gets its own name (`DropdownMenuGroupLabel`, `SelectGroupLabel`) and its own
// audit-free use site. The other half — checking that a group-scoped wrapper is nested in its group
// at every call site — would need a JSX tree walk, and every group-scoped name we have is unused in
// app code today, so it stays out rather than becoming a heuristic that fails builds mysteriously.
{
  const uiDir = path.join(root, 'src/components/ui')
  for (const name of readdirSync(uiDir).filter((f) => f.endsWith('.tsx'))) {
    const src = await readFile(path.join(uiDir, name), 'utf8')
    const exported = new Set(
      (src.match(/export {([\s\S]*?\n)}/)?.[1] ?? '').split(',').map((x) => x.trim()).filter(Boolean)
    )
    // `\n}$` rather than `\n}`: these functions open with a destructured props annotation whose
    // continuation line starts with `}` (`}: MenuPrimitive.GroupLabel.Props & {`), and a body that
    // stopped there would test the type and never the element — which is exactly how the first
    // version of this rule passed a build it was written to fail.
    for (const fn of src.matchAll(/^function (\w+Label)\b[\s\S]*?\n\}$/gm)) {
      const [body, fname] = [fn[0], fn[1]]
      if (!exported.has(fname)) continue
      // A `…GroupLabel` is *meant* to be group-scoped — that is what it is for — so it is exempt by
      // name. What is not allowed is a caption, which is what a bare `…Label` reads as to everyone
      // using the component, silently being the primitive that needs the group.
      if (fname.endsWith('GroupLabel')) continue
      // The *element*, not the type: a wrapper may well take `GroupLabel.Props` for its props and
      // render a div, which is legal and is what the fixed caption does today.
      if (/<\w+\.GroupLabel[\s/>]/.test(body)) {
        fail(`components/ui/${name}: ${fname} renders a GroupLabel primitive. A caption cannot be group-scoped — Base UI throws without the group (production error #31) and the primitive is aria-hidden. Keep the div; export a separate ${fname.replace(/Label$/, 'GroupLabel')} for grouped use.`)
      }
    }
  }
}

// ------------------------------------------------------------------ guild-page audit
// Two properties the app's own structure makes checkable, and neither is cosmetic.
//
// The rail used to render fifteen rows from three glyphs, which is the kind of thing nobody notices
// in review and everybody notices while using it — so the table has one icon per row now, and "more
// unique" is only a lasting change if a build refuses to let it decay back.
//
// The second rule is the one that would have caught the round before this one: `pages/Guild.tsx`
// picks its view from `VIEWS[page]`, so a nav row with no entry there is a tab that spins at a
// skeleton forever, and an entry with no row is a page no link can reach. Both directions are
// asserted, because the dead one is invisible and the live one is "the tabs don't work".
{
  const nav = await readFile(path.join(root, 'src/lib/nav.ts'), 'utf8')
  const rows = [...nav.matchAll(/\{ seg: '([^']*)',[^\n]*?\bicon: ([A-Za-z_]\w*)/g)]
  if (rows.length === 0) fail('nav.ts: no guild page could be read, so the icon and view rules checked nothing')
  const segs = rows.map((m) => m[1])
  const icons = rows.map((m) => m[2])
  const dupes = [...new Set(icons.filter((x, i) => icons.indexOf(x) !== i))]
  if (dupes.length) fail(`nav.ts: ${dupes.join(', ')} is used by more than one page — a glyph shared in a rail marks nothing`)
  if (new Set(segs).size !== segs.length) fail('nav.ts: a segment appears twice, so one of them can never be opened')

  const guild = await readFile(path.join(root, 'src/pages/Guild.tsx'), 'utf8')
  const views = guild.slice(guild.indexOf('const VIEWS'), guild.indexOf('\n}', guild.indexOf('const VIEWS')))
  const keys = [...views.matchAll(/(?:^|[,{\s])'?([a-z]*)&?\/?'?\s*:/g)].map((m) => m[1])
  for (const seg of segs) {
    if (!new RegExp(`^\\s*'?${seg || '\\'}'?\\s*:`, 'm').test(views) && seg !== '') {
      fail(`pages/Guild.tsx: VIEWS has no "${seg}" entry, so the ${seg} tab renders a skeleton that never resolves`)
    }
  }
  if (keys.length && keys.length !== segs.length) {
    fail(`pages/Guild.tsx: VIEWS has ${keys.length} entries for ${segs.length} nav rows — one of them is unreachable or missing`)
  }

  // A card without a glyph is a wall of text, and a page that repeats one has two of the same
  // thing; both are one-line fixes, so the build decides rather than a review comment.
  for (const file of readdirSync(path.join(root, 'src/pages/guild')).filter((f) => f.endsWith('.tsx'))) {
    const src = await readFile(path.join(root, 'src/pages/guild', file), 'utf8')
    const used = []
    for (const call of src.matchAll(/<SettingsForm\b([\s\S]*?)\n\s*\/>/g)) {
      if (!call[1].includes('fields=')) continue
      const title = call[1].match(/title="([^"]*)"/)?.[1] ?? file
      const icon = call[1].match(/icon=\{(\w+)\}/)
      if (!icon) fail(`pages/guild/${file}: the "${title}" card has no icon — every config card is identified by one`)
      else used.push(icon[1])
    }
    const again = [...new Set(used.filter((x, i) => used.indexOf(x) !== i))]
    if (again.length) fail(`pages/guild/${file}: ${again.join(', ')} is the icon of two cards on one page`)
  }
}

// ------------------------------------------------------------ settings-document audit
// The panel edits documents the bot owns. The accepted key list is the set of fields the API
// serializes for a feature, and an unknown key is a 400 rather than a shrug — deliberately, because
// every config struct is `#[serde(default)]`, so a lenient merge would store `join_threshhold`,
// answer 200, and go on raid-sensing the old number.
//
// Neither side's type checker can see the boundary: the panel writes `Record<string, unknown>` with
// a string literal, Rust sees JSON. This repo shipped 83 of them misspelled — every form
// camelCase (`welcomeEnabled`) against a model speaking snake_case (`welcome_enabled`) — and that is
// what "the features don't work and aren't customizable" actually was: not a missing feature, a 400
// on every save, for a key nobody had ever read. So the two trees are checked against each other at
// the one moment both are guaranteed to be on disk.
{
  const ws = path.resolve(root, '..')
  const modelsPath = path.join(ws, 'crates/aerona-core/src/models.rs')
  const eventsPath = path.join(ws, 'crates/aerona-bot/src/events.rs')
  if (!existsSync(modelsPath) && !process.env.AERONA_SKIP_MODEL_AUDIT) {
    fail(`crates/aerona-core/src/models.rs is not beside this dashboard, so no settings key could be checked against the model it must match. Build from the workspace root, or set AERONA_SKIP_MODEL_AUDIT=1 if this really is a dashboard-only checkout.`)
  }
  if (existsSync(modelsPath)) {
    const models = await readFile(modelsPath, 'utf8')
    const structOf = (name) => {
      const m = new RegExp(`pub struct ${name}\\s*\\{([\\s\\S]*?)\\n\\}`).exec(models)
      if (!m) fail(`models.rs has no \`pub struct ${name}\`, so the audit's feature map is stale`)
      const fields = []
      for (const f of (m ? m[1] : '').matchAll(/(?:#\[serde\([^\)]*\)\]\s*\n\s*)?pub (\w+):/g)) fields.push(f[1])
      return fields
    }
    // Kept in step with `default_doc` in the API by the assertion below, not by hope.
    const DOC = {
      general: 'GuildSettings', moderation: 'ModConfig', tickets: 'TicketConfig', sticky: 'StickyConfig',
      anti_raid: 'RaidConfig', ai_filter: 'AiConfig', link_guard: 'LinkGuardConfig',
      anti_spam: 'AntiSpamConfig', identity: 'IdentityConfig', autorole: 'AutoConfig',
      welcome: 'AutoConfig', logging: 'LogConfig', commands: 'AutoConfig',
    }
    const apiSrc = await readFile(path.join(root, 'src/lib/api.ts'), 'utf8')
    const declared = [...apiSrc.matchAll(/^\s*\| '(\w+)'$/gm)].map((m) => m[1]).filter((f) => f !== 'keys')
    for (const f of declared) if (!DOC[f]) fail(`lib/api.ts declares feature "${f}" and the audit does not know its struct — add it to DOC, next to default_doc in the API`)

    // A field with no control is a setting that exists only in the database. Each exemption below
    // says why that is correct; a new field with neither a control nor a reason fails the build.
    const EXEMPT = {
      avatar: 'staged through /identity/avatar so a megabyte of base64 never enters config_history',
      avatar_hash: 'written by the apply, not by an operator',
      categories: 'derived from categories_config by the filter page',
      categories_config: 'edited by the rule rows on the filter page, not a form field',
      custom_rules: 'edited by the rule rows on the filter page, not a form field',
      list_id: 'an allowlist row id, owned by the rules table',
    }
    /** Which fields each *document* has a control for — not each feature name, because
     *  `autorole`, `welcome` and `commands` are three screens over `AutoConfig`, and a field the
     *  Welcome page edits is not "uncontrolled" just because the Commands page does not repeat it.
     */
    const controlled = new Map()
    for (const file of readdirSync(path.join(root, 'src/pages/guild')).filter((f) => f.endsWith('.tsx'))) {
      const src = await readFile(path.join(root, 'src/pages/guild', file), 'utf8')
      const feats = new Set([...src.matchAll(/feature="(\w+)"/g)].map((m) => m[1]))
      for (const m of src.matchAll(/patch(?:Settings)?\(\s*[\w$]+,\s*'(\w+)',\s*\{/g)) feats.add(m[1])
      const keys = [...src.matchAll(/key: '(\w+)'/g)].map((m) => m[1])
      for (const m of src.matchAll(/patch(?:Settings)?\([^)]*\{\s*([\s\S]*?)\}\s*\)/g))
        for (const k of m[1].matchAll(/(?:^|[{,])\s*(\w+):/g)) keys.push(k[1])
      for (const f of feats) if (DOC[f]) for (const k of keys) {
        if (!controlled.has(DOC[f])) controlled.set(DOC[f], new Set())
        controlled.get(DOC[f]).add(k)
      }
      // A key that belongs to no feature the page edits is a form writing the wrong document.
      for (const k of keys) {
        const known = [...feats].some((f) => DOC[f] && structOf(DOC[f]).includes(k))
        if (known || /^_/.test(k)) continue
        const snake = k.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toLowerCase()
        const near = Object.keys(DOC).filter((f) => feats.has(f) && structOf(DOC[f]).includes(snake))
        // One failure per key, and the near-miss wording wins: a report that says both is a report
        // somebody has to read twice to find the difference.
        if (near.length) fail(`${file}: key '${k}' is not a field any of its features has, but '${snake}' is (${near.join('/')}) — the API will 400 the save. Spell it the model's way.`)
        else fail(`${file}: key '${k}' is not a settings field of ${[...feats].map((f) => `settings/${f}`).join(', ') || 'any feature this page edits'}`)
      }
    }
    for (const struct of new Set(Object.values(DOC))) {
      const have = controlled.get(struct) ?? new Set()
      const feats = Object.entries(DOC).filter(([, st]) => st === struct).map(([f]) => f)
      for (const f of structOf(struct)) {
        if (have.has(f) || EXEMPT[f]) continue
        const camel = f.replace(/_(\w)/g, (_, c) => c.toUpperCase())
        const note = have.has(camel) ? ` — a page calls it '${camel}', which the model does not have` : ''
        fail(`settings/${feats.join('|')} (${struct}) has '${f}', which no page controls${note}. Add a field, or exempt it in prerender.mjs with the reason.`)
      }
    }

    // A read is as wrong as a write. `data.settings.sticky?.cooldownSecs` returns `undefined`
    // silently — the tile says "off" forever and no request ever fails — so the camelCase spelling
    // of any multi-word field is refused anywhere in the panel, not only in a form. Single-word
    // fields are exempt by construction: `enabled` is a legitimate property of plenty of things.
    const allSrc = []
    for (const dir of ['pages', 'components', 'lib', 'hooks']) {
      const walk = async (d) => {
        for (const name of readdirSync(d)) {
          const f = path.join(d, name)
          if (name.endsWith('.tsx') || name.endsWith('.ts')) allSrc.push([f, await readFile(f, 'utf8')])
          else if (!name.includes('.')) await walk(f)
        }
      }
      await walk(path.join(root, 'src', dir))
    }
    // Half of these names are correct: the API's hand-built responses — the overview, the report,
    // the identity row — are camelCase by convention, and only the *settings documents* are the
    // structs' snake_case. So a camel spelling is refused unless the server really emits it, which
    // makes the rule read the tree it is checking the contract against. (Without this, `api.ts`'s
    // `strikeThreshold` and `Profile.tsx`'s `avatarHash` were flagged: both are emitted, at
    // routes/guild.rs:428 and :210.)
    const emitted = new Set()
    const collect = async (d) => {
      for (const name of readdirSync(d)) {
        const f = path.join(d, name)
        const st = await readFile(f, 'utf8').catch(() => null)
        if (st === null) { await collect(f); continue }
        if (!name.endsWith('.rs')) continue
        for (const m of st.matchAll(/"([a-z][A-Za-z0-9]*)\s*"\s*:/g)) emitted.add(m[1])
        for (const m of st.matchAll(/insert\(\s*"([a-z][A-Za-z0-9]*)"/g)) emitted.add(m[1])
      }
    }
    for (const crate of ['aerona-api', 'aerona-core', 'aerona-bot']) {
      const src = path.join(ws, 'crates', crate, 'src')
      if (existsSync(src)) await collect(src)
    }
    for (const struct of new Set(Object.values(DOC))) {
      for (const f of structOf(struct)) {
        if (!f.includes('_')) continue
        const camel = f.replace(/_(\w)/g, (_, c) => c.toUpperCase())
        if (camel === f || emitted.has(camel)) continue
        for (const [file, src] of allSrc) {
          if (new RegExp(`\\b${camel}\\b`).test(src)) {
            fail(`${path.relative(root, file)} mentions '${camel}', which is not a settings field — the model calls it '${f}', and reading the wrong name yields undefined rather than an error`)
          }
        }
      }
    }

    // Placeholders are the other half of the same boundary: the panel advertises tokens, the bot
    // substitutes them, and `{count}` in a shipped default greeting was braces to the new member.
    if (existsSync(eventsPath)) {
      const events = await readFile(eventsPath, 'utf8')
      const at = events.indexOf('fn tokens(&self)')
      if (at < 0) fail('events.rs no longer builds join tokens in `GreetingFacts::tokens`, so nothing checked the placeholders')
      const region = events.slice(at, events.indexOf('\n}', at))
      const known = new Set([...region.matchAll(/\("(\{\w+\})"/g)].map((m) => m[1]))
      if (known.size === 0) fail('no tokens could be read from GreetingFacts::tokens')
      // Only what a *sentence* advertises: a `{t}` in JSX is an expression, and the day this rule
      // was written against the whole file it failed on `{what}` in a `<dd>`.
      const wel = await readFile(path.join(root, 'src/pages/guild/Welcome.tsx'), 'utf8')
      const said = [...wel.matchAll(/'([^'\n]*)'|"([^"\n]*)"|`([^`\n]*)`/g)].map((m) => m[1] ?? m[2] ?? m[3]).join('\n')
      const advertised = new Set([...said.matchAll(/\{(\w+)\}/g)].map((m) => `{${m[1]}}`))
      for (const t of advertised) {
        if (!known.has(t)) {
          fail(`Welcome.tsx advertises ${t} in a hint or a placeholder, which the bot never replaces — a greeting that shows a new member a brace`)
        }
      }
      const defaults = [...models.matchAll(/welcome_message: "([^"]*)"|goodbye_message: "([^"]*)"/g)]
      for (const d of defaults) {
        const raw = (d[1] ?? d[2] ?? '')
        for (const t of raw.matchAll(/\{(\w+)\}/g)) {
          if (!known.has(`{${t[1]}}`)) fail(`the default greeting uses {${t[1]}}, which no substitution provides`)
        }
      }
    }
  }
}

// ------------------------------------------------------------------- the audit

const known = new Set(paths.map((p) => (p.endsWith('/') ? p : `${p}/`)))
for (const [p, html] of pages) {
  for (const m of html.matchAll(/(?:href|src)="([^"]+)"/g)) {
    const href = m[1]
    if (href.startsWith('#')) {
      if (href.length > 1 && !html.includes(`id="${href.slice(1)}"`)) fail(`${p}: anchor ${href} has no target on the page`)
      continue
    }
    if (/^(https?:|mailto:|data:|\/\/)/.test(href)) continue
    if (href.startsWith('/api/') || href.startsWith('/_')) continue
    const clean = href.split('#')[0].split('?')[0]
    if (!clean) continue
    // A font, a stylesheet or a manifest is not a route: if the file exists in `dist`,
    // the link is good. Same rule a crawler applies.
    if (clean.startsWith('/fonts/') || clean.startsWith('/assets/') || /\.\w{2,12}$/.test(clean)) {
      if (!existsSync(path.join(dist, clean))) fail(`${p}: link ${href} points at a file that was not built`)
      continue
    }
    const target = clean.endsWith('/') ? clean : `${clean}/`
    if (!known.has(target)) fail(`${p}: internal link ${href} goes nowhere`)
  }
  // Every indexable page needs a canonical, a description and a title; a private page
  // needs the opposite of the first one.
  const indexable = pagesMetaIndexability.get(p) === true
  if (indexable && !html.includes('rel="canonical"')) fail(`${p}: no canonical link`)
  if (indexable && !/<meta name="description" content="[^"]{40,}"/.test(html)) fail(`${p}: description missing or thinner than 40 characters`)
  if (!indexable && !html.includes('noindex')) fail(`${p}: a private route is indexable`)
}

for (const r of rows) {
  if (!r.h1 && r.indexable && r.path !== '/404/') fail(`${r.path}: no <h1> in the rendered markup`)
}

let graphCount = 0
for (const [p, html] of pages) {
  for (const m of html.matchAll(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/g)) {
    graphCount++
    let parsed = null
    try {
      parsed = JSON.parse(m[1])
    } catch (e) {
      fail(`${p}: JSON-LD does not parse (${e.message})`)
      continue
    }
    const graph = parsed?.['@graph']
    if (!Array.isArray(graph) || graph.length === 0) fail(`${p}: empty JSON-LD graph`)
    else {
      if (!graph.some((n) => n?.['@type'] === 'WebSite')) fail(`${p}: graph has no WebSite node`)
      const indexable = pagesMetaIndexability.get(p) === true
      if (indexable && p !== '/' && !graph.some((n) => n?.['@type'] === 'BreadcrumbList')) {
        fail(`${p}: no BreadcrumbList for an indexable page`)
      }
    }
  }
}

// ------------------------------------------------------------- sidecar files

// Same rule as `lib/meta.ts`: pages take a trailing slash, files keep their name.
const abs = (p) => origin + (/\.\w{2,8}$/.test(p) ? p : p === '/' ? '/' : p.endsWith('/') ? p : `${p}/`)
await writeFile(
  path.join(dist, 'robots.txt'),
  // There is nothing to allow: every route on this origin is the panel, behind a Discord
  // session, and a crawler that indexes `/tickets/` indexes somebody's moderation queue.
  ['User-agent: *', 'Disallow: /', '', '# A self-hosted Aerona install is a private panel, not a website. There is no public page', '# to crawl and no sitemap worth reading; set AERONA_SITE_ORIGIN at build time only if you', '# publish a copy of this dashboard somewhere else.'].join('\n'),
  'utf8',
)
const indexableRows = rows.filter((r) => r.indexable)
// A build date, not a fabricated per-page one: a `lastmod` that lies is worse than no
// `lastmod`, because a crawler stops trusting the file.
const builtAt = new Date().toISOString().slice(0, 10)
await writeFile(
  path.join(dist, 'sitemap.xml'),
  [
    '<?xml version="1.0" encoding="UTF-8"?>',
    '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">',
    ...indexableRows.map((r) => `  <url><loc>${abs(r.path)}</loc><lastmod>${builtAt}</lastmod></url>`),
    '</urlset>',
    '',
  ].join('\n'),
  'utf8',
)
// A build must not silently produce a one-line robots.txt. `join('\n')` with a doubled
// backslash is the exact typo that does it — valid JS, and the file reads as literal
// backslash-n sequences to every crawler — so the output is verified, not assumed.
// `llms.txt` used to be checked here too, and shipped a page list that described a front
// page; a private panel has nothing to advertise to an agent, so the file is gone.
for (const name of ['robots.txt']) {
  const body = await readFile(path.join(dist, name), 'utf8')
  if (body.includes(String.fromCharCode(92) + 'n')) fail(`${name} contains literal \\n sequences instead of newlines`)
  if (body.split('\n').length < 4) fail(`${name} has fewer than 4 lines (join produced a single line)`)
}

// ------------------------------------------------------------------- summary

console.log('\n  prerendered routes')
for (const r of rows) {
  console.log(`  ${r.indexable ? '·' : 'i'} ${r.path.padEnd(20)} ${(r.bytes / 1024).toFixed(1).padStart(7)} KB`)
}
console.log(`  i ${'/shell.html'.padEnd(20)} ${(shellBytes / 1024).toFixed(1).padStart(7)} KB  (the app shell for /guild/<id>/…)`)
console.log(
  `  ${rows.length} routes (${indexableRows.length} indexable), 1 shell, ${graphCount} JSON-LD graph(s), sitemap lastmod ${builtAt}${origin === 'https://aerona.local' ? ', canonicals use the placeholder origin (set AERONA_SITE_ORIGIN)' : ''}`,
)
if (problems.length) {
  console.error('\n  audit failed:')
  for (const p of problems) console.error(`   ✗ ${p}`)
  console.error('')
  process.exit(1)
}
console.log('  audit clean\n')
