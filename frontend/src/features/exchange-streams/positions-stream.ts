import { type RustPositionFrame, RustPositionFrameSchema } from '@tiny/domain'

import { createSharedStream, createStreamRegistry, type SharedStream } from './shared-stream'
import { openDemoSocket } from './socket'
import { exchangeSocketUrl } from './socket-url'

export interface PositionsState {
  status: 'connecting' | 'live' | 'disconnected' | 'unknown_account'
  /** The latest snapshot; the flat fields describe the keyed series. */
  snapshot: RustPositionFrame | undefined
}

export const IDLE_POSITIONS: PositionsState = { status: 'connecting', snapshot: undefined }

/**
 * One socket per account and series to `/ws/positions`. Each frame is absolute, rebuilt by
 * the exchange on every book or trade broadcast. An unknown account gets one error frame
 * and a closed socket, which the store reports rather than retrying into.
 */
function createPositionsStream(
  apiUrl: string,
  address: string,
  series: string,
  onRelease: () => void
): SharedStream<PositionsState> {
  return createSharedStream<PositionsState>(
    IDLE_POSITIONS,
    (publish, current) => {
      const socket = openDemoSocket(
        exchangeSocketUrl(apiUrl, '/ws/positions', { address, series }),
        {
          opened: () => undefined,
          message: payload => {
            const parsed = RustPositionFrameSchema.safeParse(payload)
            if (parsed.success) {
              publish({ status: 'live', snapshot: parsed.data })
              return
            }
            if (
              typeof payload === 'object' &&
              payload !== null &&
              'error' in payload &&
              payload.error === 'unknown_account'
            ) {
              publish({ ...current(), status: 'unknown_account' })
            }
          },
          disconnected: () => {
            const state = current()
            if (state.status !== 'unknown_account') publish({ ...state, status: 'disconnected' })
          },
        }
      )
      return { close: () => socket.close() }
    },
    onRelease
  )
}

const registry = createStreamRegistry<SharedStream<PositionsState>>((key, release) => {
  const [apiUrl = '', address = '', series = ''] = key.split('|')
  return createPositionsStream(apiUrl, address, series, release)
})

export function getPositionsStream(apiUrl: string, address: string, series: string) {
  return registry(`${apiUrl}|${address}|${series}`)
}
