import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import {
  OPEN_TX,
  RECORD_ACCOUNT,
  RECORD_DEPLOYMENT_ID,
  recordView,
  seal,
  sealTx,
} from './record.fixture'
import { RecordLine } from './record-line'

const server = setupServer()
const apiUrl = 'http://exchange.test'
/** Time for an answered request to settle into a render. */
const ANSWER_SETTLE_MS = 50

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
})
afterAll(() => server.close())

function answer(body: unknown, status = 200) {
  server.use(
    http.get(`${apiUrl}/api/records/${RECORD_ACCOUNT}`, () =>
      HttpResponse.json(body as object, { status })
    )
  )
}

function renderLine(client = new QueryClient()) {
  return render(
    <QueryClientProvider client={client}>
      <RecordLine address={RECORD_ACCOUNT} apiUrl={apiUrl} />
    </QueryClientProvider>
  )
}

describe('RecordLine', () => {
  it('renders nothing for a deployment the exchange holds no record for', async () => {
    let answered = false
    server.events.on('response:mocked', () => {
      answered = true
    })
    answer({ error: 'not_found', message: 'no record' }, 404)
    const { container } = renderLine()

    await waitFor(() => expect(answered).toBe(true))
    // Let the answer reach the screen; a failure would show "Record unavailable" by now.
    await act(() => new Promise(resolve => setTimeout(resolve, ANSWER_SETTLE_MS)))
    expect(screen.queryByText(/Record/)).not.toBeInTheDocument()
    expect(container).toBeEmptyDOMElement()
    server.events.removeAllListeners('response:mocked')
  })

  it('says the record is pending while the opening is not on chain', async () => {
    answer(recordView(null, []))
    renderLine()

    expect(await screen.findByText('Record pending')).toBeInTheDocument()
    expect(screen.queryByRole('link')).not.toBeInTheDocument()
  })

  it('names a single sealed round in the singular', async () => {
    answer(recordView(OPEN_TX, [seal(5, true)]))
    renderLine()

    expect(await screen.findByText('Record sealed · 1 round')).toBeInTheDocument()
  })

  it('counts pending rounds and links the latest sealed round', async () => {
    answer(recordView(OPEN_TX, [seal(9, false), seal(7, true), seal(8, true)]))
    const { container } = renderLine()

    expect(await screen.findByText('Record sealed · 2 rounds · 1 pending')).toBeInTheDocument()
    const explorer = screen.getByRole('link', {
      name: "View on Arc: this strategy's record on the Arc explorer",
    })
    expect(explorer).toHaveAttribute('href', `https://explorer.testnet.arc.io/tx/${sealTx(8)}`)
    expect(explorer).toHaveAttribute('target', '_blank')
    expect(explorer).toHaveAttribute('rel', 'noopener noreferrer')
    const journal = screen.getByRole('link', {
      name: 'Journal: the latest sealed round, line by line',
    })
    expect(journal).toHaveAttribute(
      'href',
      `${apiUrl}/api/records/${RECORD_DEPLOYMENT_ID}/8/journal`
    )
    expect(journal).toHaveAttribute('target', '_blank')
    expect(journal).toHaveAttribute('rel', 'noopener noreferrer')
    await expectNoA11yViolations(container)
  })

  it('says the record is unavailable when the exchange fails', async () => {
    answer({ error: 'internal', message: 'boom' }, 500)
    renderLine()

    expect(await screen.findByText('Record unavailable')).toBeInTheDocument()
  })
})
