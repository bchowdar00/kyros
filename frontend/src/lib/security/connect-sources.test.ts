import { describe, expect, it } from 'vitest'

import { publicConnectSources } from './connect-sources'

describe('publicConnectSources', () => {
  it('returns unique origins with their socket twins and skips missing URLs', () => {
    expect(
      publicConnectSources(
        'https://api.tiny.xyz/v1',
        undefined,
        'http://localhost:8080/ws/price',
        'http://localhost:8080'
      )
    ).toEqual([
      'https://api.tiny.xyz',
      'wss://api.tiny.xyz',
      'http://localhost:8080',
      'ws://localhost:8080',
    ])
  })
})
