/** Which step of the request lifecycle failed. Callers branch on this, never on message text. */
export type ApiFailureStep = 'http' | 'network' | 'parse' | 'timeout'

export class ApiError extends Error {
  override readonly name = 'ApiError'
  readonly step: ApiFailureStep
  readonly status: number | undefined
  readonly retryable: boolean

  constructor(
    step: ApiFailureStep,
    message: string,
    options: { cause?: unknown; retryable: boolean; status?: number } = { retryable: false }
  ) {
    super(message, options.cause === undefined ? undefined : { cause: options.cause })
    this.step = step
    this.status = options.status
    this.retryable = options.retryable
  }
}

/** Requests never throw into UI code. They return one of these. */
export type ApiResult<Data> =
  | { readonly ok: true; readonly data: Data }
  | { readonly ok: false; readonly error: ApiError }

export function isRetryableStatus(status: number): boolean {
  return status === 408 || status === 425 || status === 429 || status >= 500
}
