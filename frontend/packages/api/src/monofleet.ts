import { ExchangeAuthSchema } from '@kyros/domain'
import { ApiError, type ApiResult } from './errors'

/** Local development adapter for the token published by monofleet's served UI. */
export async function loadMonofleetSession(uiUrl: string): Promise<ApiResult<string>> {
  const url = new URL(uiUrl)
  if (url.protocol !== 'http:' || !['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)) {
    return { ok: false, error: new ApiError('parse', 'Monofleet must be on loopback') }
  }
  try {
    const response = await fetch(url, {
      cache: 'no-store',
      redirect: 'error',
      signal: AbortSignal.timeout(3000),
    })
    if (!response.ok) return { ok: false, error: new ApiError('http', 'Monofleet UI unavailable') }
    // Match only the exact literal emitted by ui.rs; never evaluate upstream JavaScript.
    const html = await response.text()
    const marker = "const TOKEN_FRAME = '"
    const start = html.indexOf(marker)
    const literal = start < 0 ? '' : html.slice(start + marker.length, start + marker.length + 2000)
    const end = literal.indexOf("';")
    const frame = end < 0 ? '' : literal.slice(0, end)
    const parsed = ExchangeAuthSchema.safeParse(JSON.parse(frame) as unknown)
    if (!parsed.success) return { ok: false, error: new ApiError('parse', 'Invalid demo session') }
    return { ok: true, data: frame }
  } catch {
    return { ok: false, error: new ApiError('network', 'Demo exchange unavailable') }
  }
}
