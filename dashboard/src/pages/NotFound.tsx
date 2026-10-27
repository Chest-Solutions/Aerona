import { pagesMeta } from '../pages-meta'
import { Link } from '../router'
import { Button } from '@/components/ui/button'
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty'

export const meta = pagesMeta.NotFound

/** What an unknown address looks like when the whole product is one app: a sentence and the
 *  way back. Rendered by the API for any path it cannot find, which is why it is a route and
 *  not an error page in a template - a 404 that differs between the server and the client is
 *  a 404 an operator has to debug twice. */
export default function NotFound() {
  return (
    <div className="flex min-h-svh items-center justify-center px-5">
      <Empty>
        <EmptyHeader>
          <EmptyMedia variant="icon">404</EmptyMedia>
          <EmptyTitle>Nothing at this address</EmptyTitle>
          <EmptyDescription>
            The dashboard is one page per server. Every server you administer is on the list.
          </EmptyDescription>
        </EmptyHeader>
        <EmptyContent>
          <Button render={<Link href="/" />}>All servers</Button>
        </EmptyContent>
      </Empty>
    </div>
  )
}
