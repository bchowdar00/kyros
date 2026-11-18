import { render, screen, waitFor, within } from '@testing-library/react'
import { expectNoA11yViolations } from '@tiny/testing/a11y'
import type { UIMessage } from 'ai'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest'

import { ChatDataProvider } from '@/features/chat/bindings/chat-data'
import { WidgetToolPart } from '@/features/chat/render/widget-tool-part'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const apiUrl = 'http://exchange.test'

/** The catalogue names signals; the table reads the English name from it. */
const catalogue = {
  series: 'btc-2m',
  underlying: 'btc',
  signals: [
    {
      signal_id: 'btc.kraken.roc.v1',
      name: 'Rate of change',
      description: 'Percent change over the window.',
      location: 'internal',
      scope: { kind: 'underlying', underlying: 'btc' },
      lane: 'spot',
      live: true,
      quality: { state: 'ready', reasons: [] },
      fields: [{ name: 'roc', label: 'ROC', type: 'decimal', headline: true }],
    },
  ],
}

type MessagePart = UIMessage['parts'][number]

function toolPart(state: string, extra: Record<string, unknown>): MessagePart {
  return { type: 'tool-showChart', toolCallId: 'call-1', state, ...extra } as MessagePart
}

describe('WidgetToolPart', () => {
  it('shows a skeleton while the call streams and the reason when the tool refused', async () => {
    const { container, rerender } = render(
      <WidgetToolPart part={toolPart('input-streaming', { input: {} })} onEvent={vi.fn()} />
    )
    expect(screen.getByRole('region', { name: 'Widget' })).toHaveAttribute('aria-busy', 'true')
    await expectNoA11yViolations(container)

    rerender(
      <WidgetToolPart
        part={toolPart('output-available', {
          input: {},
          output: { ok: false, reason: 'nope.v1 is not a signal of btc-2m.' },
        })}
        onEvent={vi.fn()}
      />
    )
    expect(screen.getByRole('status')).toHaveTextContent('nope.v1 is not a signal of btc-2m.')

    rerender(
      <WidgetToolPart
        part={toolPart('output-error', { input: {}, errorText: 'Timed out' })}
        onEvent={vi.fn()}
      />
    )
    expect(screen.getByRole('status')).toHaveTextContent('Timed out')
  })

  it('draws authored content inline and marks it as written by Teeny', async () => {
    const { container } = render(
      <WidgetToolPart
        part={toolPart('output-available', {
          input: {},
          output: {
            ok: true,
            spec: { kind: 'markdown', title: 'Read', text: 'RSI is **oversold** at 28.' },
          },
        })}
        onEvent={vi.fn()}
      />
    )
    expect(screen.getByRole('region', { name: 'Read' })).toBeInTheDocument()
    expect(screen.getByText('oversold')).toBeInTheDocument()
    expect(screen.getByText('Written by Teeny, not live data.')).toBeInTheDocument()
    await expectNoA11yViolations(container)
  })

  it('draws an authored table with each cell under its column', async () => {
    server.use(http.get(`${apiUrl}/api/signals`, () => HttpResponse.json(catalogue)))
    const { container } = render(
      <ChatDataProvider value={{ apiUrl, series: 'btc-2m', address: null, timeZone: 'UTC' }}>
        <WidgetToolPart
          part={toolPart('output-available', {
            input: {},
            output: {
              ok: true,
              spec: {
                kind: 'table',
                title: 'Momentum signals',
                columns: [
                  { id: 'id', label: 'Signal' },
                  { id: 'venue', label: 'Venue' },
                  { id: 'ready', label: 'Ready' },
                ],
                rows: [
                  ['btc.kraken.roc.v1', 'Kraken', 'yes'],
                  ['btc.binance.roc.v1', 'Binance', 'no'],
                ],
              },
            },
          })}
          onEvent={vi.fn()}
        />
      </ChatDataProvider>
    )
    const table = screen.getByRole('table')
    const rows = within(table).getAllByRole('row')
    // Header plus two data rows.
    expect(rows).toHaveLength(3)
    // A signal id the catalogue lists reads as its venue and server-authored name; one it
    // does not list stays as written.
    await waitFor(() => {
      const kraken = within(rows[1] as HTMLElement).getAllByRole('cell')
      expect(kraken.map(cell => cell.textContent)).toEqual([
        'Kraken rate of change',
        'Kraken',
        'yes',
      ])
    })
    const binance = within(rows[2] as HTMLElement).getAllByRole('cell')
    expect(binance.map(cell => cell.textContent)).toEqual(['btc.binance.roc.v1', 'Binance', 'no'])
    await expectNoA11yViolations(container)
  })

  it('refuses a malformed spec and a bound widget outside an exchange', () => {
    const { rerender } = render(
      <WidgetToolPart
        part={toolPart('output-available', { input: {}, output: { kind: 'gauge' } })}
        onEvent={vi.fn()}
      />
    )
    expect(screen.getByRole('status')).toHaveTextContent('could not be read')

    rerender(
      <WidgetToolPart
        part={toolPart('output-available', {
          input: {},
          output: {
            ok: true,
            spec: { kind: 'chart', series: 'btc-2m', refs: [{ type: 'spot' }], title: 'Spot' },
          },
        })}
        onEvent={vi.fn()}
      />
    )
    expect(screen.getByRole('region', { name: 'Spot' })).toHaveTextContent(
      'not connected to an exchange'
    )
  })

  it('sends a picker choice back as a sentence', async () => {
    const onEvent = vi.fn()
    const { default: userEvent } = await import('@testing-library/user-event')
    render(
      <WidgetToolPart
        part={toolPart('output-available', {
          input: {},
          output: {
            ok: true,
            spec: {
              kind: 'picker',
              prompt: 'Which side?',
              options: [
                { id: 'yes', title: 'YES at 62¢' },
                { id: 'no', title: 'NO at 40¢' },
              ],
            },
          },
        })}
        onEvent={onEvent}
      />
    )
    const user = userEvent.setup()
    await user.click(screen.getByRole('radio', { name: /NO at 40¢/ }))
    await user.click(screen.getByRole('button', { name: 'Continue' }))
    expect(onEvent).toHaveBeenCalledWith('Chose: NO at 40¢.')
  })
})
