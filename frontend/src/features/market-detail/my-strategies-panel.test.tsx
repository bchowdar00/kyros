import { cents } from '@kyros/money'
import { cleanup, render, screen, within } from '@testing-library/react'
import { http, HttpResponse } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import { MyStrategiesPanel } from './my-strategies-panel'
import { OPEN_TX, RECORD_ACCOUNT, recordView, seal } from './record/record.fixture'
import type { MyStrategiesData } from './types'

const server = setupServer()
const apiUrl = 'http://exchange.test'
const MAIN_ACCOUNT = `0x${'11'.repeat(20)}`

const panel: MyStrategiesData = {
  title: 'My Accounts',
  tabs: [{ id: 'strategies', label: 'My Accounts' }],
  rows: [
    { id: MAIN_ACCOUNT, name: 'Main', status: 'active', totalPnlCents: cents(0), trades: 0 },
    {
      id: RECORD_ACCOUNT,
      name: 'Late Closer',
      status: 'active',
      totalPnlCents: cents(125),
      trades: 3,
    },
  ],
}

// Unhandled requests fail the test: the Main row asking for a record would be one.
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
})
afterAll(() => server.close())

describe('MyStrategiesPanel record lines', () => {
  it("shows a strategy's record under its name and never asks for the main account's", async () => {
    const asked: string[] = []
    server.use(
      http.get(`${apiUrl}/api/records/:id`, ({ params }) => {
        asked.push(String(params['id']))
        return HttpResponse.json(recordView(OPEN_TX, [seal(4, true)]))
      })
    )
    render(<MyStrategiesPanel {...panel} apiUrl={apiUrl} />)

    const strategyRow = screen.getByRole('row', { name: /Late Closer/ })
    expect(await within(strategyRow).findByText('Record sealed · 1 round')).toBeInTheDocument()
    const mainRow = screen.getByRole('row', { name: /Main/ })
    expect(within(mainRow).queryByText(/^Record/)).not.toBeInTheDocument()
    expect(asked).toEqual([RECORD_ACCOUNT])
  })

  it('requests no record without an exchange url', () => {
    render(<MyStrategiesPanel {...panel} />)

    expect(screen.getByText('Late Closer')).toBeInTheDocument()
    expect(screen.queryByText(/^Record/)).not.toBeInTheDocument()
  })
})
