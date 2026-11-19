import { describe, expect, it } from 'vitest'

import { isOwnOrigin } from './own-origin'

const SITE = 'https://kyros.example'

function post(url: string, origin?: string) {
  return new Request(url, { method: 'POST', headers: origin === undefined ? {} : { origin } })
}

describe('isOwnOrigin', () => {
  it('accepts a request whose origin is the address the server sees', () => {
    expect(isOwnOrigin(post('http://localhost:3000/api/chat', 'http://localhost:3000'), SITE)).toBe(
      true
    )
  })

  it("accepts the site's public origin when a proxy hands the server its own address", () => {
    // Behind a host's ingress the server builds the request URL from the address it listens
    // on, while the browser sends the public origin.
    expect(isOwnOrigin(post('https://localhost:3000/api/chat', SITE), SITE)).toBe(true)
  })

  it("rejects another site's origin", () => {
    expect(isOwnOrigin(post('https://localhost:3000/api/chat', 'https://evil.example'), SITE)).toBe(
      false
    )
  })

  it('rejects the public host on another scheme or port', () => {
    expect(isOwnOrigin(post('https://localhost:3000/api/chat', 'http://kyros.example'), SITE)).toBe(
      false
    )
    expect(
      isOwnOrigin(post('https://localhost:3000/api/chat', 'https://kyros.example:8443'), SITE)
    ).toBe(false)
  })

  it('rejects a request with no origin', () => {
    expect(isOwnOrigin(post('https://localhost:3000/api/chat'), SITE)).toBe(false)
  })
})
