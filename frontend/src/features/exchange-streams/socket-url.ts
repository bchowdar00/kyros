const SOCKET_SCHEME: Readonly<Record<string, string>> = { 'http:': 'ws:', 'https:': 'wss:' }

/** `/ws/*` lives on the same host as the REST API; only the scheme changes. */
export function exchangeSocketUrl(
  apiUrl: string,
  path: string,
  query: Readonly<Record<string, string>> = {}
): string {
  const url = new URL(path, apiUrl)
  url.protocol = SOCKET_SCHEME[url.protocol] ?? url.protocol

  for (const [key, value] of Object.entries(query)) {
    url.searchParams.set(key, value)
  }

  return url.toString()
}
