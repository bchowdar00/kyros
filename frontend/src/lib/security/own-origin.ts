/**
 * Whether a state-changing request was sent by one of our own pages: its `Origin` header is
 * either the origin the server sees for the request or the site's public origin (`siteUrl`,
 * from `NEXT_PUBLIC_SITE_URL`). Behind a host's ingress the two differ: the server builds the
 * request URL from the address it listens on, while the browser sends the public origin.
 */
export function isOwnOrigin(request: Request, siteUrl: string): boolean {
  const origin = request.headers.get('origin')
  if (origin === null) return false
  return origin === new URL(request.url).origin || origin === new URL(siteUrl).origin
}
