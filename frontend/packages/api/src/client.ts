import { ApiError, type ApiResult, isRetryableStatus } from './errors'
import { DEFAULT_RETRY_POLICY, defaultSleep, retryDelayMs, type RetryPolicy } from './retry'
import { describeIssues, type InferOutput, type StandardSchema } from './schema'

export type SearchParams = Record<string, boolean | number | string | undefined>

export interface ApiClientOptions {
  baseUrl: string
  /** Injected for tests and for server code that wants Next's patched fetch. */
  fetch?: typeof fetch
  /** Sent with every request; per-request headers win on conflict. */
  headers?: HeadersInit
  retry?: Partial<RetryPolicy>
  /** Injected for tests so retry delays do not slow the suite down. */
  sleep?: (ms: number) => Promise<void>
  timeoutMs?: number
}

export interface GetOptions<Schema extends StandardSchema> {
  headers?: HeadersInit
  schema: Schema
  searchParams?: SearchParams
  signal?: AbortSignal
}

export interface PostOptions<Schema extends StandardSchema> extends GetOptions<Schema> {
  body: unknown
  /**
   * Makes the request safe to retry: the server deduplicates on this key. Every mutation
   * that moves money must carry one. Without it a POST is attempted exactly once.
   */
  idempotencyKey?: string
}

export interface ApiClient {
  get<Schema extends StandardSchema>(
    path: string,
    options: GetOptions<Schema>
  ): Promise<ApiResult<InferOutput<Schema>>>
  post<Schema extends StandardSchema>(
    path: string,
    options: PostOptions<Schema>
  ): Promise<ApiResult<InferOutput<Schema>>>
}

const DEFAULT_TIMEOUT_MS = 10_000

export function createApiClient(options: ApiClientOptions): ApiClient {
  const fetchImpl = options.fetch ?? globalThis.fetch
  const sleep = options.sleep ?? defaultSleep
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS
  const policy: RetryPolicy = { ...DEFAULT_RETRY_POLICY, ...options.retry }
  const baseUrl = options.baseUrl.endsWith('/') ? options.baseUrl : `${options.baseUrl}/`

  function buildUrl(path: string, searchParams: SearchParams | undefined): URL {
    const url = new URL(path.replace(/^\//, ''), baseUrl)

    for (const [key, value] of Object.entries(searchParams ?? {})) {
      if (value !== undefined) {
        url.searchParams.set(key, String(value))
      }
    }

    return url
  }

  function buildHeaders(
    perRequest: HeadersInit | undefined,
    extra: Record<string, string>
  ): Headers {
    const headers = new Headers(options.headers)
    new Headers(perRequest).forEach((value, key) => headers.set(key, value))

    for (const [key, value] of Object.entries(extra)) {
      headers.set(key, value)
    }

    return headers
  }

  async function attempt<Schema extends StandardSchema>(
    request: Request,
    schema: Schema
  ): Promise<ApiResult<InferOutput<Schema>>> {
    let response: Response

    try {
      response = await fetchImpl(request)
    } catch (cause) {
      const isTimeout = cause instanceof DOMException && cause.name === 'TimeoutError'

      return {
        ok: false,
        error: new ApiError(
          isTimeout ? 'timeout' : 'network',
          isTimeout ? `Request timed out after ${timeoutMs}ms` : 'Network request failed',
          { cause, retryable: true }
        ),
      }
    }

    if (!response.ok) {
      return {
        ok: false,
        error: new ApiError('http', `${response.status} ${response.statusText}`.trim(), {
          retryable: isRetryableStatus(response.status),
          status: response.status,
        }),
      }
    }

    let payload: unknown

    try {
      payload = await response.json()
    } catch (cause) {
      return {
        ok: false,
        error: new ApiError('parse', 'Response body is not JSON', {
          cause,
          retryable: false,
          status: response.status,
        }),
      }
    }

    const result = await schema['~standard'].validate(payload)

    if (result.issues) {
      return {
        ok: false,
        error: new ApiError('parse', describeIssues(result.issues), {
          retryable: false,
          status: response.status,
        }),
      }
    }

    return { ok: true, data: result.value as InferOutput<Schema> }
  }

  async function run<Schema extends StandardSchema>(
    build: () => Request,
    schema: Schema,
    canRetry: boolean
  ): Promise<ApiResult<InferOutput<Schema>>> {
    const startedAt = Date.now()
    let attemptIndex = 0

    for (;;) {
      const result = await attempt(build(), schema)

      if (result.ok || !canRetry || !result.error.retryable) {
        return result
      }

      const delay = retryDelayMs(attemptIndex, policy)
      const elapsed = Date.now() - startedAt
      const isLastAttempt = attemptIndex + 1 >= policy.attempts
      const wouldExceedBudget = elapsed + delay > policy.budgetMs

      if (isLastAttempt || wouldExceedBudget) {
        return result
      }

      await sleep(delay)
      attemptIndex += 1
    }
  }

  function combineSignals(signal: AbortSignal | undefined): AbortSignal {
    const timeout = AbortSignal.timeout(timeoutMs)

    return signal ? AbortSignal.any([signal, timeout]) : timeout
  }

  return {
    get(path, requestOptions) {
      const url = buildUrl(path, requestOptions.searchParams)

      return run(
        () =>
          new Request(url, {
            headers: buildHeaders(requestOptions.headers, { accept: 'application/json' }),
            method: 'GET',
            signal: combineSignals(requestOptions.signal),
          }),
        requestOptions.schema,
        true
      )
    },

    post(path, requestOptions) {
      const url = buildUrl(path, requestOptions.searchParams)
      const body = JSON.stringify(requestOptions.body)
      const idempotency =
        requestOptions.idempotencyKey === undefined
          ? {}
          : { 'idempotency-key': requestOptions.idempotencyKey }

      return run(
        () =>
          new Request(url, {
            body,
            headers: buildHeaders(requestOptions.headers, {
              accept: 'application/json',
              'content-type': 'application/json',
              ...idempotency,
            }),
            method: 'POST',
            signal: combineSignals(requestOptions.signal),
          }),
        requestOptions.schema,
        requestOptions.idempotencyKey !== undefined
      )
    },
  }
}
