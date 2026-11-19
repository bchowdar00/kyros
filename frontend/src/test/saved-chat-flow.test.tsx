import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { HttpResponse, http, delay } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { ChatPanel } from '@/features/market-detail/chat-panel'
import { MARKET_DETAIL_FIXTURES } from '@/features/market-detail/market-detail.fixture'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())
const panel = MARKET_DETAIL_FIXTURES['btc-above-118k-0903'].chatPanel
const saved = { id: 'saved-chat', title: 'BTC price', updatedAt: '2026-09-05T12:00:00Z' }

describe('saved chat navigation', () => {
  it('restores default copy, opens persisted history, goes back, and creates a separately titled chat', async () => {
    server.use(
      http.get('*/api/chats', () => HttpResponse.json({ chats: [saved], hasMore: false })),
      http.get('*/api/chats/saved-chat', () =>
        HttpResponse.json({
          chat: saved,
          messages: [
            { id: 'old', role: 'user', parts: [{ type: 'text', text: 'My saved question' }] },
          ],
        })
      ),
      http.post('*/api/chat', async ({ request }) => {
        const body = (await request.json()) as { id: string; marketSlug: string }
        expect(body.id).not.toBe(saved.id)
        expect(body.marketSlug).toBe('btc')
        const chunks = [
          { type: 'start', messageId: 'answer' },
          { type: 'text-start', id: 't' },
          { type: 'text-delta', id: 't', delta: 'A new answer' },
          { type: 'text-end', id: 't' },
          { type: 'finish' },
        ]
        return new HttpResponse(
          `${chunks.map(chunk => `data: ${JSON.stringify(chunk)}\n\n`).join('')}data: [DONE]\n\n`,
          {
            headers: { 'content-type': 'text/event-stream', 'x-vercel-ai-ui-message-stream': 'v1' },
          }
        )
      })
    )
    const user = userEvent.setup()
    render(<ChatPanel data={panel} marketSlug="btc" />)
    expect(
      screen.getByRole('heading', { name: /^Buy Up when BTC is 10 basis points above the strike/ })
    ).toBeVisible()
    expect(screen.getByRole('button', { name: 'Build this strategy' })).toBeEnabled()
    await user.click(await screen.findByRole('button', { name: 'Open chat: BTC price' }))
    expect(await screen.findByText('My saved question')).toBeVisible()
    expect(screen.getByRole('heading', { name: 'BTC price' })).toBeVisible()
    await user.click(screen.getByRole('button', { name: 'Back to chats and public strategies' }))
    expect(
      await screen.findByRole('heading', {
        name: /^Buy Up when BTC is 10 basis points above the strike/,
      })
    ).toBeVisible()
    expect(screen.getByRole('heading', { name: panel.sectionTitle })).toBeVisible()
    await user.type(
      screen.getByRole('textbox', { name: 'Message Kyros' }),
      'A different question{Enter}'
    )
    expect(await screen.findByText('A new answer')).toBeVisible()
    expect(screen.getByRole('heading', { name: 'A different question' })).toBeVisible()
    expect(screen.queryByText('My saved question')).not.toBeInTheDocument()
  })
  it('keeps an in-progress chat as a card and reopens it without dropping the reply', async () => {
    server.use(
      http.post('*/api/chat', async () => {
        await delay(300)
        const chunks = [
          { type: 'start', messageId: 'answer' },
          { type: 'text-start', id: 't' },
          { type: 'text-delta', id: 't', delta: 'Still here' },
          { type: 'text-end', id: 't' },
          { type: 'finish' },
        ]
        return new HttpResponse(
          `${chunks.map(chunk => `data: ${JSON.stringify(chunk)}\n\n`).join('')}data: [DONE]\n\n`,
          {
            headers: { 'content-type': 'text/event-stream', 'x-vercel-ai-ui-message-stream': 'v1' },
          }
        )
      })
    )
    const user = userEvent.setup()
    render(<ChatPanel data={panel} />)
    await user.type(screen.getByRole('textbox', { name: 'Message Kyros' }), 'Keep this chat{Enter}')
    expect(screen.getByRole('textbox', { name: 'Message Kyros' })).toHaveFocus()
    await user.click(screen.getByRole('button', { name: 'Back to chats and public strategies' }))
    expect(
      screen.getByRole('heading', { name: /^Buy Up when BTC is 10 basis points above the strike/ })
    ).toHaveFocus()
    await user.click(screen.getByRole('button', { name: 'Open chat: Keep this chat' }))
    expect(await screen.findByText('Still here')).toBeVisible()
    expect(screen.getAllByRole('article', { name: 'You' })).toHaveLength(1)
  })

  it('deletes a saved chat only after confirmation, and keeps it when cancelled', async () => {
    let chats = [saved, { id: 'other-chat', title: 'ETH question', updatedAt: saved.updatedAt }]
    let deletes = 0
    server.use(
      http.get('*/api/chats', () => HttpResponse.json({ chats, hasMore: false })),
      http.delete('*/api/chats/saved-chat', () => {
        deletes += 1
        chats = chats.filter(chat => chat.id !== saved.id)
        return new HttpResponse(null, { status: 204 })
      })
    )
    const user = userEvent.setup()
    render(<ChatPanel data={panel} marketSlug="btc" />)
    await user.click(await screen.findByRole('button', { name: 'Delete chat: BTC price' }))
    const dialog = screen.getByRole('dialog', { name: 'BTC price' })
    expect(dialog).toBeVisible()
    expect(screen.getByRole('button', { name: 'Keep chat' })).toHaveFocus()
    await user.click(screen.getByRole('button', { name: 'Keep chat' }))
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
    expect(deletes).toBe(0)
    expect(screen.getByRole('button', { name: 'Open chat: BTC price' })).toBeVisible()

    await user.click(screen.getByRole('button', { name: 'Delete chat: BTC price' }))
    await user.click(screen.getByRole('button', { name: 'Delete chat' }))
    await waitFor(() =>
      expect(screen.queryByRole('button', { name: 'Open chat: BTC price' })).not.toBeInTheDocument()
    )
    expect(deletes).toBe(1)
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Open chat: ETH question' })).toBeVisible()
  })

  it('keeps the dialog open with a message when deleting fails', async () => {
    server.use(
      http.get('*/api/chats', () => HttpResponse.json({ chats: [saved], hasMore: false })),
      http.delete('*/api/chats/saved-chat', () => new HttpResponse(null, { status: 503 }))
    )
    const user = userEvent.setup()
    render(<ChatPanel data={panel} marketSlug="btc" />)
    await user.click(await screen.findByRole('button', { name: 'Delete chat: BTC price' }))
    await user.click(screen.getByRole('button', { name: 'Delete chat' }))
    await waitFor(() =>
      expect(screen.getByRole('alert')).toHaveTextContent('The chat couldn’t be deleted')
    )
    expect(screen.getByRole('dialog', { name: 'BTC price' })).toBeVisible()
    expect(screen.getByRole('button', { name: 'Open chat: BTC price' })).toBeVisible()
  })

  it('keeps the home view usable when loading saved chats fails', async () => {
    server.use(http.get('*/api/chats', () => new HttpResponse(null, { status: 503 })))
    render(<ChatPanel data={panel} marketSlug="btc" />)
    await waitFor(() =>
      expect(screen.getByRole('alert')).toHaveTextContent('Saved chats couldn’t be loaded')
    )
    expect(screen.getByRole('textbox', { name: 'Message Kyros' })).toBeEnabled()
  })
})
