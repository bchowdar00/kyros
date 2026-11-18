const SOCKET_SCHEME: Readonly<Record<string, string>> = { 'http:': 'ws:', 'https:': 'wss:' }

/**
 * Origins the browser may call besides `'self'`. Duplicates collapse. Each http(s) origin
 * also allows its ws(s) twin, since browsers treat the socket scheme as a separate origin
 * in `connect-src` and the exchange serves both from one host.
 */
export function publicConnectSources(
  ...urls: ReadonlyArray<string | undefined>
): readonly string[] {
  const origins = new Set<string>()

  for (const value of urls) {
    if (value === undefined) {
      continue
    }

    const url = new URL(value)
    origins.add(url.origin)
    const socketScheme = SOCKET_SCHEME[url.protocol]

    if (socketScheme !== undefined) {
      origins.add(`${socketScheme}//${url.host}`)
    }
  }

  return [...origins]
}
