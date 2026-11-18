import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, expect, it } from 'vitest'
import { loadMonofleetSession } from './monofleet'
const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())
it('reads the served demo token without executing JavaScript', async () => {
  const frame = JSON.stringify({
    version: 6,
    type: 'auth',
    body: {
      account: '1',
      expires_at: '1788610000000000000',
      key_id: 1,
      cancel_on_disconnect_ms: 0,
      sig: '0'.repeat(128),
    },
  })
  server.use(
    http.get('http://127.0.0.1:8090/', () =>
      HttpResponse.html(`<script>const TOKEN_FRAME = '${frame}';</script>`)
    )
  )
  expect(await loadMonofleetSession('http://127.0.0.1:8090')).toEqual({ ok: true, data: frame })
})
it('fails closed when the HTML contract changes or points off machine', async () => {
  server.use(http.get('http://127.0.0.1:8090/', () => HttpResponse.html('<p>unavailable</p>')))
  expect((await loadMonofleetSession('http://127.0.0.1:8090')).ok).toBe(false)
  expect((await loadMonofleetSession('https://exchange.example')).ok).toBe(false)
})
