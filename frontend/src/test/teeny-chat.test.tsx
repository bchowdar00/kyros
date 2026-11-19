import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { HttpResponse, http, delay } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { parseChatRequest } from '@/features/market-detail/chat-request'
import { ChatPanel } from '@/features/market-detail/chat-panel'
import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'

const server = setupServer()
const panel = MARKET_DETAIL_FIXTURES['btc-above-118k-0903'].chatPanel
const answer = `${[
  { type: 'start', messageId: 'answer' },
  { type: 'text-start', id: 'text' },
  { type: 'text-delta', id: 'text', delta: 'Hello from Kyros.' },
  { type: 'text-end', id: 'text' },
  { type: 'finish' },
]
  .map(chunk => `data: ${JSON.stringify(chunk)}\n\n`)
  .join('')}data: [DONE]\n\n`

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

describe('Kyros chat', () => {
  it('replaces public strategies with the submitted message and streamed answer', async () => {
    server.use(
      http.post(
        '*/api/chat',
        () =>
          new HttpResponse(answer, {
            headers: { 'content-type': 'text/event-stream', 'x-vercel-ai-ui-message-stream': 'v1' },
          })
      )
    )
    const user = userEvent.setup()
    render(<ChatPanel data={panel} />)
    expect(screen.getByRole('heading', { name: panel.sectionTitle })).toBeVisible()
    await user.type(screen.getByRole('textbox', { name: 'Message Kyros' }), 'Hello{Enter}')
    expect(screen.queryByRole('heading', { name: panel.sectionTitle })).not.toBeInTheDocument()
    expect(screen.getByRole('article', { name: 'You' })).toHaveTextContent('Hello')
    expect(await screen.findByText('Hello from Kyros.')).toBeVisible()
    await waitFor(() => expect(screen.getByRole('button', { name: 'Send message' })).toBeDisabled())
  })

  it('shows tool activity and sends a valid text-only follow-up after tool execution', async () => {
    let attempts = 0
    const toolStart = [
      { type: 'start', messageId: 'tool-answer' },
      { type: 'start-step' },
      {
        type: 'tool-input-available',
        toolCallId: 'quote',
        toolName: 'getLivePrice',
        input: { symbol: 'Crypto.BTC/USD' },
      },
    ]
      .map(chunk => `data: ${JSON.stringify(chunk)}\n\n`)
      .join('')
    server.use(
      http.post('*/api/chat', async ({ request }) => {
        attempts += 1
        const body = await request.text()
        expect(parseChatRequest(body)).toBeDefined()
        if (attempts > 1) {
          expect(body).not.toContain('tool-getLivePrice')
          return new HttpResponse(answer, {
            headers: { 'content-type': 'text/event-stream', 'x-vercel-ai-ui-message-stream': 'v1' },
          })
        }
        return new HttpResponse(
          new ReadableStream({
            async start(controller) {
              const encoder = new TextEncoder()
              controller.enqueue(encoder.encode(toolStart))
              await delay(150)
              const end = [
                {
                  type: 'tool-output-available',
                  toolCallId: 'quote',
                  output: { status: 'unavailable' },
                },
                { type: 'finish-step' },
                { type: 'start-step' },
                { type: 'text-start', id: 'reply' },
                { type: 'text-delta', id: 'reply', delta: 'The price feed is unavailable.' },
                { type: 'text-end', id: 'reply' },
                { type: 'finish-step' },
                { type: 'finish' },
              ]
                .map(chunk => `data: ${JSON.stringify(chunk)}\n\n`)
                .join('')
              controller.enqueue(encoder.encode(`${end}data: [DONE]\n\n`))
              controller.close()
            },
          }),
          {
            headers: { 'content-type': 'text/event-stream', 'x-vercel-ai-ui-message-stream': 'v1' },
          }
        )
      })
    )
    const user = userEvent.setup()
    render(<ChatPanel data={panel} />)
    await user.type(screen.getByRole('textbox', { name: 'Message Kyros' }), 'BTC price?{Enter}')
    expect(await screen.findByText('Checking price feeds…')).toBeVisible()
    expect(await screen.findByText('The price feed is unavailable.')).toBeVisible()
    await user.type(screen.getByRole('textbox', { name: 'Message Kyros' }), 'Try again{Enter}')
    expect(await screen.findByText('Hello from Kyros.')).toBeVisible()
    expect(attempts).toBe(2)
  })

  it('retains the user message on failure and retries without duplicating it', async () => {
    let attempts = 0
    server.use(
      http.post('*/api/chat', () => {
        attempts += 1
        return attempts === 1
          ? new HttpResponse(null, { status: 503 })
          : new HttpResponse(answer, {
              headers: {
                'content-type': 'text/event-stream',
                'x-vercel-ai-ui-message-stream': 'v1',
              },
            })
      })
    )
    const user = userEvent.setup()
    render(<ChatPanel data={panel} />)
    await user.type(screen.getByRole('textbox', { name: 'Message Kyros' }), 'Hello{Enter}')
    expect(await screen.findByRole('alert')).toHaveTextContent('couldn’t finish')
    await user.click(screen.getByRole('button', { name: 'Try again' }))
    expect(await screen.findByText('Hello from Kyros.')).toBeVisible()
    expect(screen.getAllByRole('article', { name: 'You' })).toHaveLength(1)
  })
  it('stops a pending response without clearing a new draft', async () => {
    server.use(
      http.post('*/api/chat', async () => {
        await delay(1000)
        return new HttpResponse(answer, {
          headers: { 'content-type': 'text/event-stream', 'x-vercel-ai-ui-message-stream': 'v1' },
        })
      })
    )
    const user = userEvent.setup()
    render(<ChatPanel data={panel} />)
    await user.type(screen.getByRole('textbox', { name: 'Message Kyros' }), 'Hello{Enter}')
    const input = screen.getByRole('textbox', { name: 'Message Kyros' })
    await user.type(input, 'My next question')
    await user.click(screen.getByRole('button', { name: 'Stop response' }))
    await waitFor(() => expect(screen.getByRole('button', { name: 'Send message' })).toBeEnabled())
    expect(input).toHaveValue('My next question')
    expect(screen.getByRole('article', { name: 'You' })).toHaveTextContent('Hello')
  })

  it("renders Kyros's Markdown as prose and keeps the user's asterisks literal", async () => {
    const reply = `${[
      { type: 'start', messageId: 'formatted' },
      { type: 'text-start', id: 'text' },
      {
        type: 'text-delta',
        id: 'text',
        delta: '**Edge by regime** means:\n\n- **Quiet** ranges\n- Strong trends',
      },
      { type: 'text-end', id: 'text' },
      { type: 'finish' },
    ]
      .map(chunk => `data: ${JSON.stringify(chunk)}\n\n`)
      .join('')}data: [DONE]\n\n`
    server.use(
      http.post(
        '*/api/chat',
        () =>
          new HttpResponse(reply, {
            headers: { 'content-type': 'text/event-stream', 'x-vercel-ai-ui-message-stream': 'v1' },
          })
      )
    )
    const user = userEvent.setup()
    render(<ChatPanel data={panel} />)
    await user.type(screen.getByRole('textbox', { name: 'Message Kyros' }), 'what is 2 ** 3{Enter}')
    expect(screen.getByRole('article', { name: 'You' })).toHaveTextContent('what is 2 ** 3')

    const teeny = await screen.findByRole('article', { name: 'Kyros' })
    expect(
      within(teeny)
        .getAllByRole('listitem')
        .map(li => li.textContent)
    ).toEqual(['Quiet ranges', 'Strong trends'])
    expect(teeny).toHaveTextContent('Edge by regime means:')
    expect(teeny).not.toHaveTextContent('*')
    expect(within(teeny).getByText('Edge by regime').tagName).toBe('STRONG')
  })
})
