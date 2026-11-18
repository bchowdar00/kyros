import { expect, test } from '@playwright/test'

test('the existing BTC screen streams depth and submits UP and DOWN demo orders', async ({
  page,
  baseURL,
}, testInfo) => {
  expect(baseURL).toMatch(/^http:\/\/(localhost|127\.0\.0\.1):\d+$/)
  page.on('pageerror', error => console.error(error.message))
  const orders: Array<{ side: string; kind: string; price: number; qty: string }> = []
  page.on('websocket', socket => {
    if (!socket.url().endsWith('/v1/ws')) return
    socket.on('framesent', event => {
      const frame = JSON.parse(String(event.payload))
      if (frame.type === 'new_order') orders.push(frame.body)
    })
  })
  await page.goto('/market/btc-updown-2m')
  await expect(page.getByRole('heading', { name: 'BTC Up or Down 2m' })).toBeVisible()
  const connection = page.getByRole('region', { name: 'Exchange connection' })
  await expect(connection).toContainText('Local simulated trading')
  await expect(connection).toContainText('Feed: live · Account: live')
  const book = page.getByRole('table', { name: 'Order book' })
  await expect(book.getByRole('row').nth(2)).toBeVisible()
  for (const [index, outcome] of ['UP', 'DOWN'].entries()) {
    await page.getByRole('button', { name: new RegExp(`^Buy ${outcome} outcome at`) }).click()
    const ticket = page.getByRole('dialog', { name: 'Buy outcome' })
    await ticket.getByRole('textbox', { name: 'Order amount in USD' }).fill('1')
    await expect(ticket.getByRole('button', { name: 'Buy', exact: true })).toBeEnabled()
    await ticket.getByRole('button', { name: 'Buy', exact: true }).click()
    await expect.poll(() => orders.length).toBe(index + 1)
    expect(orders[index]).toMatchObject({ side: outcome === 'UP' ? 'buy' : 'sell', kind: 'ioc' })
    await expect(ticket).toContainText(/Filled \d+ lots|Order cancelled\./)
    await page.keyboard.press('Escape')
    await expect(ticket).not.toBeVisible()
  }
  await page.getByRole('tab', { name: 'Trade History', exact: true }).click()
  await expect(page.getByRole('region', { name: 'Simulated trade history' })).toContainText(
    'Your latest fills this visit'
  )
  await page.evaluate(() => window.scrollTo({ top: 0, behavior: 'instant' }))
  await page.screenshot({ path: testInfo.outputPath('btc-exchange-1440.png'), fullPage: true })
})
