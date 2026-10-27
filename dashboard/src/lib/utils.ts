export { cn } from "cn"

/** `/tickets` → `/tickets/`. Every route here is a directory holding an `index.html`, because
 *  that is what a static file server can serve without rewrite rules - so the trailing slash is
 *  part of the URL, not decoration, and it is applied in one place. */
export function withSlash(path: string): string {
  if (path === '' || path === '/') return '/'
  return path.endsWith('/') ? path : `${path}/`
}

