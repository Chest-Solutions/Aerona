/**
 * Per-page SEO data. A page module exports `meta()` returning this; the prerenderer
 * turns it into `<title>`, description, canonical, OG/Twitter and a JSON-LD graph,
 * and the client applies the same data on navigation so the two never disagree.
 */
export type MetaKind = 'website' | 'WebPage' | 'SoftwareApplication' | 'TechArticle' | 'FAQPage' | 'ItemList'

export type PageMeta = {
  title: string
  description: string
  path: string
  kind: MetaKind
  /** Section titles for the breadcrumb trail, home first. */
  crumbs?: { label: string; href: string }[]
  /** App and auth pages exist for one operator: they must not be indexed. */
  noindex?: boolean
  updated?: string
  ogTitle?: string
  faq?: { q: string; a: string }[]
  /** Alt text for the share image. Defaults to a composed line in `head.ts`. */
  ogAlt?: string
}

export const HOME_CRUMB = { label: 'Aerona', href: '/' }

/**
 * Canonical URLs end in a trailing slash, per the guidelines - but only for *pages*.
 * A file keeps its name: `/og-image.jpg/` is a 404 on every static host, and a
 * prerender audit that resolves the URL catches it immediately (it did).
 */
export function canonical(origin: string, path: string): string {
  const isFile = /\.\w{2,8}$/.test(path)
  const p = isFile || path.endsWith('/') ? path : `${path}/`
  return `${origin.replace(/\/$/, '')}${p === '//' ? '/' : p}`
}
