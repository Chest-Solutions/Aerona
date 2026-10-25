import { site } from '../site'
import { canonical } from './meta'
import type { PageMeta } from './meta'

/** The `@graph` for one page: Organization + WebSite + WebPage, plus BreadcrumbList
 *  and, where the page earns it, SoftwareApplication / FAQPage / ItemList. */
export function graph(origin: string, meta: PageMeta) {
  const url = canonical(origin, meta.path)
  const nodes: Record<string, unknown>[] = [
    {
      '@type': 'Organization',
      '@id': `${origin}/#organization`,
      name: site.legalName,
      url: canonical(origin, '/'),
      description: site.description,
      contactPoint: [{ '@type': 'ContactPoint', contactType: 'support', email: site.email.replace('mailto:', '') }],
      // Entity disambiguation: the repository is the only identity this project has.
      ...(site.github ? { sameAs: [site.github] } : {}),
    },
    {
      '@type': 'WebSite',
      '@id': `${origin}/#website`,
      url: canonical(origin, '/'),
      name: site.name,
      description: site.tagline,
      publisher: { '@id': `${origin}/#organization` },
    },
    {
      '@type': [meta.kind === 'website' ? 'WebPage' : meta.kind],
      '@id': url,
      url,
      name: meta.title,
      description: meta.description,
      isPartOf: { '@id': `${origin}/#website` },
      about: { '@id': `${origin}/#organization` },
      image: { '@type': 'ImageObject', url: `${canonical(origin, '/og-image.jpg')}`, width: 1200, height: 630 },
      inLanguage: site.locale,
      ...(meta.updated ? { dateModified: meta.updated } : {}),
    },
  ]

  const crumbs = [{ label: 'Home', href: '/' }, ...(meta.crumbs ?? [])].map((c) => ({
    '@type': 'ListItem',
    name: c.label,
    item: canonical(origin, c.href),
  }))
  if (crumbs.length > 1) {
    nodes.push({ '@type': 'BreadcrumbList', '@id': `${url}#breadcrumb`, itemListElement: crumbs })
  }

  // The application node belongs to the root only: `/` is the dashboard itself now that
  // the install has no marketing pages, and it is the one document a crawler is ever sent to.
  if (meta.path === '/') {
    nodes.push({
      '@type': 'SoftwareApplication',
      '@id': `${canonical(origin, '/')}#app`,
      name: site.name,
      applicationCategory: 'BusinessApplication',
      applicationSubCategory: 'Community moderation',
      operatingSystem: 'Linux, macOS',
      softwareVersion: '1.0.0',
      downloadUrl: site.github,
      codeRepository: site.github,
      license: { '@type': 'SoftwareSourceCode', name: 'MIT' },
      offers: { '@type': 'Offer', price: '0', priceCurrency: 'USD' },
    })
  }
  if (meta.faq?.length) {
    nodes.push({
      '@type': 'FAQPage',
      '@id': `${url}#faq`,
      mainEntity: meta.faq.map((f) => ({
        '@type': 'Question',
        name: f.q,
        acceptedAnswer: { '@type': 'Answer', text: f.a },
      })),
    })
  }
  return { '@context': 'https://schema.org', '@graph': nodes }
}
