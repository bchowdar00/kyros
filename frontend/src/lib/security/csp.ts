export interface ContentSecurityPolicyOptions {
  /** Extra origins the browser may fetch or open sockets to: the API, the realtime gateway. */
  connectSources?: readonly string[]
  isDevelopment: boolean
  /**
   * Whether the page itself was served over HTTPS. `upgrade-insecure-requests` is only valid
   * there; on a plain-HTTP origin (local production runs, e2e) it rewrites every asset URL to
   * https and the page loads nothing.
   */
  isSecureOrigin: boolean
  nonce: string
}

/** A fresh, unguessable nonce per request. Base64 keeps it safe inside the header value. */
export function createNonce(): string {
  return btoa(crypto.randomUUID())
}

/**
 * Strict, nonce-based policy. Scripts run only with this request's nonce, and
 * `'strict-dynamic'` lets those scripts load the chunks they import. Nothing inline
 * executes without the nonce, which Next.js applies to its own tags automatically.
 *
 * Styles allow `'unsafe-inline'` on purpose: server-rendered `style` attributes (order-book
 * depth bars, CSS variables carrying data) are blocked by a nonce-only policy and React
 * does not re-apply them on hydration. Style injection is a far smaller risk than script
 * injection and is revisited once every data-driven style goes through a CSS variable.
 */
export function buildContentSecurityPolicy({
  connectSources = [],
  isDevelopment,
  isSecureOrigin,
  nonce,
}: ContentSecurityPolicyOptions): string {
  const directives: ReadonlyArray<readonly [name: string, values: readonly string[]]> = [
    ['default-src', ["'self'"]],
    [
      'script-src',
      [
        "'self'",
        `'nonce-${nonce}'`,
        "'strict-dynamic'",
        ...(isDevelopment ? ["'unsafe-eval'"] : []),
      ],
    ],
    ['style-src', ["'self'", "'unsafe-inline'"]],
    ['img-src', ["'self'", 'blob:', 'data:']],
    ['font-src', ["'self'"]],
    ['connect-src', ["'self'", ...connectSources, ...(isDevelopment ? ['ws:', 'wss:'] : [])]],
    ['worker-src', ["'self'", 'blob:']],
    ['frame-ancestors', ["'self'"]],
    ['object-src', ["'none'"]],
    ['base-uri', ["'self'"]],
    ['form-action', ["'self'"]],
    ...(isSecureOrigin ? [['upgrade-insecure-requests', []] as const] : []),
  ]

  return directives
    .map(([name, values]) => (values.length === 0 ? name : `${name} ${values.join(' ')}`))
    .join('; ')
}
