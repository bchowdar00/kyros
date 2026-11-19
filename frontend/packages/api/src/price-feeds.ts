import { FeedAssetsSchema, FeedHistorySchema, FeedTickSchema, type FeedTick } from '@kyros/domain'
import { createApiClient, type ApiClientOptions } from './client'
import { ApiError, type ApiResult } from './errors'

// A chat read waits for one observation, not a permanent monitoring subscription.
const LIVE_READ_TIMEOUT_MS = 5_000
const MAX_STREAM_CHARACTERS = 65_536

export function createPriceFeedsClient(options: ApiClientOptions) {
  const client = createApiClient(options)
  return {
    listAssets(signal?: AbortSignal) {
      return client.get('/v1/assets', {
        schema: FeedAssetsSchema,
        searchParams: { active: true },
        ...(signal ? { signal } : {}),
      })
    },
    getHistory(symbol: string, from: number, to: number, signal?: AbortSignal) {
      return client.get('/v1/prices', {
        schema: FeedHistorySchema,
        searchParams: { symbol, from, to, resolution: 60 },
        ...(signal ? { signal } : {}),
      })
    },
    getLivePrice(symbol: string, signal?: AbortSignal) {
      return readLivePrice(options, symbol, signal)
    },
  }
}

async function readLivePrice(
  options: ApiClientOptions,
  symbol: string,
  signal?: AbortSignal
): Promise<ApiResult<FeedTick>> {
  const url = new URL('/v1/stream/prices', options.baseUrl)
  url.searchParams.set('symbols', symbol)
  const timeout = AbortSignal.timeout(options.timeoutMs ?? LIVE_READ_TIMEOUT_MS)
  const connection = new AbortController()
  const abort = AbortSignal.any([connection.signal, timeout, ...(signal ? [signal] : [])])
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined
  try {
    const response = await (options.fetch ?? globalThis.fetch)(url, {
      headers: { ...Object.fromEntries(new Headers(options.headers)), accept: 'text/event-stream' },
      signal: abort,
      cache: 'no-store',
      redirect: 'error',
    })
    if (!response.ok) {
      return {
        ok: false,
        error: new ApiError('http', 'Price feed unavailable', {
          status: response.status,
          retryable: false,
        }),
      }
    }
    if (!response.headers.get('content-type')?.includes('text/event-stream') || !response.body) {
      return { ok: false, error: new ApiError('parse', 'Invalid price stream') }
    }
    reader = response.body.getReader()
    const decoder = new TextDecoder()
    let buffer = ''
    let received = 0
    for (;;) {
      const chunk = await reader.read()
      if (chunk.done) return { ok: false, error: new ApiError('network', 'Price stream ended') }
      const text = decoder.decode(chunk.value, { stream: true })
      received += text.length
      if (received > MAX_STREAM_CHARACTERS)
        return { ok: false, error: new ApiError('parse', 'Price stream too large') }
      buffer += text
      // Price Feeds emits one JSON data line per event. Handle fragmented and CRLF frames.
      let newline = buffer.indexOf('\n')
      while (newline !== -1) {
        const line = buffer.slice(0, newline).trimEnd()
        buffer = buffer.slice(newline + 1)
        if (line.startsWith('data:')) {
          const parsed = FeedTickSchema.safeParse(JSON.parse(line.slice(5).trimStart()))
          if (!parsed.success || parsed.data.symbol !== symbol)
            return { ok: false, error: new ApiError('parse', 'Invalid price observation') }
          return { ok: true, data: parsed.data }
        }
        newline = buffer.indexOf('\n')
      }
    }
  } catch {
    return {
      ok: false,
      error: new ApiError(abort.aborted ? 'timeout' : 'network', 'Live price unavailable'),
    }
  } finally {
    // Abort also closes an unexpected response body. Do not await cancellation:
    // a fetch interceptor or a teed response can keep that promise pending.
    connection.abort()
    if (reader) {
      void reader.cancel().catch(() => undefined)
      reader.releaseLock()
    }
  }
}
