import { site } from '../site'
import { canonical } from './meta'
import type { PageMeta } from './meta'
import { graph } from './jsonld'

const esc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;')

/** `og:type` is not decoration: a share card for a doc page should behave like an
 *  article (and accept `article:*`), while a product page should not. */
const OG_TYPE = (kind: PageMeta['kind']) => (kind === 'TechArticle' || kind === 'FAQPage' ? 'article' : 'website')

/** Everything that goes between `<head>` tags except the static ones in index.html.
 *
 *  Two rules the build audit enforces: an indexable page carries a canonical and a
 *  description of at least 40 characters, and a private page carries neither — a
 *  `rel=canonical` on a `noindex` route tells a crawler "this is the preferred URL" in
 *  the same breath as "stay away", and the two are irreconcilable. `og:image:alt` is on
 *  every social page because a missing alt is the one share-preview tag that silently
 *  degrades a card into a grey box for screen-reader users.
 */
export function headTags(origin: string, meta: PageMeta): string {
  const url = canonical(origin, meta.path)
  const g = graph(origin, meta)
  const indexable = !meta.noindex
  const rows: string[] = [
    `<title>${esc(meta.title)}</title>`,
    `<meta name="description" content="${esc(meta.description)}" />`,
    `<meta name="application-name" content="${esc(site.name)}" />`,
    // One palette, so exactly one answer: this is what a scrollbar, a spinner or a native
    // select is painted with *before* the stylesheet arrives, and `light dark` here would give a
    // light-OS reader light form controls on a near-black field for a frame.
    `<meta name="color-scheme" content="dark" />`,
    `<meta name="robots" content="${indexable ? 'index,follow,max-image-preview:large,max-snippet:-1' : 'noindex,nofollow'}" />`,
    ...(indexable ? [`<link rel="canonical" href="${url}" />`] : []),
  ]
  if (indexable) {
    rows.push(
      `<meta property="og:type" content="${OG_TYPE(meta.kind)}" />`,
      `<meta property="og:site_name" content="${esc(site.name)}" />`,
      `<meta property="og:locale" content="${ogLocale(site.locale)}" />`,
      `<meta property="og:title" content="${esc(meta.ogTitle ?? site.ogTitle)}" />`,
      `<meta property="og:description" content="${esc(meta.description)}" />`,
      `<meta property="og:url" content="${url}" />`,
      `<meta property="og:image" content="${canonical(origin, '/og-image.jpg')}" />`,
      `<meta property="og:image:width" content="1200" />`,
      `<meta property="og:image:height" content="630" />`,
      `<meta property="og:image:alt" content="${esc(meta.ogAlt ?? `${site.name} — ${site.tagline}`)}" />`,
      `<meta name="twitter:card" content="summary_large_image" />`,
      `<meta name="twitter:title" content="${esc(meta.ogTitle ?? site.ogTitle)}" />`,
      `<meta name="twitter:description" content="${esc(meta.description)}" />`,
      `<meta name="twitter:image" content="${canonical(origin, '/og-image.jpg')}" />`,
      `<meta name="twitter:image:alt" content="${esc(meta.ogAlt ?? `${site.name} — ${site.tagline}`)}" />`,
    )
    if (meta.updated) {
      rows.push(`<meta property="article:modified_time" content="${iso(meta.updated)}" />`)
    }
  }
  rows.push(`<script type="application/ld+json">${JSON.stringify(g).replace(/</g, '\\u003c')}</script>`)
  return rows.join('\n    ')
}

/** `site.locale` is a BCP-47 tag; `og:locale` wants the underscore form. */
function ogLocale(tag: string): string {
  const [lang, region] = tag.split('-')
  return region ? `${lang?.toLowerCase()}_${region.toUpperCase()}` : `${lang ?? 'en'}_US`
}

/** `meta.updated` is authored as a plain date; `article:modified_time` wants a datetime. */
function iso(v: string): string {
  return /^\d{4}-\d{2}-\d{2}$/.test(v) ? `${v}T00:00:00Z` : v
}

/** The client applies the same fields after navigation. */
export function applyHead(meta: PageMeta): void {
  if (typeof document === 'undefined') return
  document.title = meta.title
  const set = (sel: string, attrs: Record<string, string>) => {
    let el = document.head.querySelector<HTMLMetaElement | HTMLLinkElement>(sel)
    if (!el) {
      el = document.createElement(sel.startsWith('link') ? 'link' : 'meta')
      document.head.appendChild(el)
    }
    for (const [k, v] of Object.entries(attrs)) el!.setAttribute(k, v)
  }
  set('meta[name="description"]', { name: 'description', content: meta.description })
  set('meta[name="robots"]', { name: 'robots', content: meta.noindex ? 'noindex,nofollow' : 'index,follow' })
  if (!meta.noindex) set('link[rel="canonical"]', { rel: 'canonical', href: canonical(site.origin, meta.path) })
}
