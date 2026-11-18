import type { NextConfig } from 'next'

// Static security headers. The Content-Security-Policy is per request (it carries a nonce)
// and is set in src/proxy.ts.
const securityHeaders = [
  { key: 'Strict-Transport-Security', value: 'max-age=63072000; includeSubDomains; preload' },
  { key: 'X-Content-Type-Options', value: 'nosniff' },
  { key: 'Referrer-Policy', value: 'strict-origin-when-cross-origin' },
  {
    key: 'Permissions-Policy',
    value: 'camera=(), microphone=(), geolocation=(), browsing-topics=(), interest-cohort=()',
  },
]

const nextConfig: NextConfig = {
  reactCompiler: true,
  typedRoutes: true,
  poweredByHeader: false,
  transpilePackages: ['@tiny/ui'],
  images: {
    // Only the widths the layouts use. The default list generates eight srcset candidates.
    deviceSizes: [390, 430, 768, 1024, 1440, 1920],
    imageSizes: [20, 36, 40, 48, 96, 256],
    minimumCacheTTL: 86_400,
  },
  headers: () => Promise.resolve([{ source: '/(.*)', headers: securityHeaders }]),
}

export default nextConfig
