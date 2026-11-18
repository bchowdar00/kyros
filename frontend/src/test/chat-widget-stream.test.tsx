import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { HttpResponse, http, delay } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'

import { useIdentityStore } from '@/features/identity/identity-store'
import { ChatPanel } from '@/features/market-detail/chat-panel'
import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'

const address = `0x${'ab'.repeat(20)}`

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())
const panel = MARKET_DETAIL_FIXTURES['nba-bos-la-083126'].chatPanel

const sse = (chunks: object[]) =>
  `${chunks.map(chunk => `data: ${JSON.stringify(chunk)}\n\n`).join('')}data: [DONE]\n\n`
const headers = { 'content-type': 'text/event-stream', 'x-vercel-ai-ui-message-stream': 'v1' }

describe('widgets in the transcript', () => {
  it('shows a skeleton while a show tool streams, then the widget the tool returned', async () => {
    useIdentityStore.setState({
      identity: { address, username: 'jet', color: 'blue', emoji: '🐙' },
    })
    let body: { accountAddress?: string } = {}
    server.use(
      http.post('*/api/chat', async ({ request }) => {
        body = (await request.json()) as { accountAddress?: string }
        return new HttpResponse(
          new ReadableStream({
            async start(controller) {
              const encoder = new TextEncoder()
              controller.enqueue(
                encoder.encode(
                  [
                    { type: 'start', messageId: 'answer' },
                    { type: 'start-step' },
                    {
                      type: 'tool-input-start',
                      toolCallId: 'draw',
                      toolName: 'showContent',
                    },
                  ]
                    .map(chunk => `data: ${JSON.stringify(chunk)}\n\n`)
                    .join('')
                )
              )
              await delay(100)
              controller.enqueue(
                encoder.encode(
                  sse([
                    {
                      type: 'tool-input-available',
                      toolCallId: 'draw',
                      toolName: 'showContent',
                      input: { kind: 'markdown', text: 'x' },
                    },
                    {
                      type: 'tool-output-available',
                      toolCallId: 'draw',
                      output: {
                        ok: true,
                        spec: { kind: 'markdown', title: 'The read', text: 'RSI is **low**.' },
                      },
                    },
                    { type: 'finish-step' },
                    { type: 'start-step' },
                    { type: 'text-start', id: 'reply' },
                    { type: 'text-delta', id: 'reply', delta: 'Here is the read.' },
                    { type: 'text-end', id: 'reply' },
                    { type: 'finish-step' },
                    { type: 'finish' },
                  ])
                )
              )
              controller.close()
            },
          }),
          { headers }
        )
      })
    )
    const user = userEvent.setup()
    render(<ChatPanel data={panel} />)
    await user.type(screen.getByRole('textbox', { name: 'Message Teeny' }), 'Show me RSI{Enter}')
    expect(await screen.findByText('Preparing a chart…')).toBeVisible()
    expect(await screen.findByRole('region', { name: 'The read' })).toBeVisible()
    expect(screen.getByText('low')).toBeVisible()
    expect(await screen.findByText('Here is the read.')).toBeVisible()
    expect(screen.queryByText('Preparing a chart…')).not.toBeInTheDocument()
    expect(body.accountAddress).toBe(address)
    useIdentityStore.setState({ identity: null })
  })

  it('draws the widget again when a saved chat is reopened', async () => {
    const saved = { id: 'saved-widget', title: 'RSI read', updatedAt: '2026-09-05T12:00:00Z' }
    server.use(
      http.get('*/api/chats', () => HttpResponse.json({ chats: [saved], hasMore: false })),
      http.get('*/api/chats/saved-widget', () =>
        HttpResponse.json({
          chat: saved,
          messages: [
            { id: 'q', role: 'user', parts: [{ type: 'text', text: 'Show me the read' }] },
            {
              id: 'a',
              role: 'assistant',
              parts: [
                {
                  type: 'tool-showContent',
                  toolCallId: 'draw',
                  state: 'output-available',
                  input: {},
                  output: {
                    ok: true,
                    spec: { kind: 'markdown', title: 'Saved read', text: 'Still **low**.' },
                  },
                },
                { type: 'text', text: 'As before.' },
              ],
            },
          ],
        })
      )
    )
    const user = userEvent.setup()
    render(<ChatPanel data={panel} marketSlug="btc" />)
    await user.click(await screen.findByRole('button', { name: 'Open chat: RSI read' }))
    expect(await screen.findByRole('region', { name: 'Saved read' })).toBeVisible()
    expect(screen.getByText('As before.')).toBeVisible()
  })
})
