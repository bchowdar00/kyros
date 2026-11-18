import { type NextRequest, NextResponse } from 'next/server'

import { env } from '@/env'
import { publicConnectSources } from '@/lib/security/connect-sources'
import { buildContentSecurityPolicy, createNonce } from '@/lib/security/csp'

/**
 * Runs before every page render. Today it does one thing: mint the CSP nonce for this
 * request and hand it to Next.js through the request headers so the framework stamps it
 * onto its script tags. Add further concerns as separate, tested functions, not here.
 */
export function proxy(request: NextRequest) {
  const nonce = createNonce()
  const policy = buildContentSecurityPolicy({
    connectSources: publicConnectSources(env.NEXT_PUBLIC_API_URL, env.NEXT_PUBLIC_PRICE_FEEDS_URL),
    isDevelopment: env.NODE_ENV === 'development',
    isSecureOrigin:
      request.nextUrl.protocol === 'https:' || request.headers.get('x-forwarded-proto') === 'https',
    nonce,
  })

  const requestHeaders = new Headers(request.headers)
  requestHeaders.set('x-nonce', nonce)
  requestHeaders.set('content-security-policy', policy)

  const response = NextResponse.next({ request: { headers: requestHeaders } })
  response.headers.set('content-security-policy', policy)

  return response
}

export const config = {
  matcher: [
    {
      // Everything except static assets and Next internals; prefetches carry no HTML.
      source: '/((?!_next/static|_next/image|assets|favicon.ico).*)',
      missing: [
        { type: 'header', key: 'next-router-prefetch' },
        { type: 'header', key: 'purpose', value: 'prefetch' },
      ],
    },
  ],
}
