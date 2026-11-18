import { AxeBuilder } from '@axe-core/playwright'
import { expect, test } from '@playwright/test'

test('Bitcoin demos advance in real time and keep a stationary tooltip through a price shock', async ({
  page,
}, testInfo) => {
  await page.clock.install()
  await page.goto('/charts')
  await page.clock.pauseAt(new Date((await page.evaluate(() => Date.now())) + 1_000))
  const demo = page.getByRole('region', { name: 'Bitcoin chart demos' })
  await expect(demo.getByRole('button', { name: 'Pause demo' })).toBeVisible()
  await demo.getByRole('combobox', { name: 'Demo scenario' }).selectOption('rally')
  await demo.getByRole('button', { name: 'Replay demo' }).click()
  await page.clock.runFor(250)
  const slider = demo.getByRole('slider')
  const [initialStart, initialEnd] = await slider.evaluate(
    element =>
      [
        Number(element.getAttribute('aria-valuemin')),
        Number(element.getAttribute('aria-valuemax')),
      ] as const
  )
  await slider.hover({ position: { x: 400, y: 100 } })
  const tooltip = demo.getByRole('tooltip')
  const selected = await slider.getAttribute('aria-valuetext')
  const before = await tooltip.locator('text').first().boundingBox()
  await page.clock.runFor(4_500)
  const [advancedStart, advancedEnd] = await slider.evaluate(
    element =>
      [
        Number(element.getAttribute('aria-valuemin')),
        Number(element.getAttribute('aria-valuemax')),
      ] as const
  )
  expect(initialEnd - initialStart).toBe(60_000)
  expect(advancedEnd - advancedStart).toBe(initialEnd - initialStart)
  expect(advancedStart).toBeGreaterThan(initialStart)
  expect(advancedEnd - initialEnd).toBeGreaterThan(4_000)
  expect(advancedEnd - initialEnd).toBeLessThan(5_000)
  await expect(slider).toHaveAttribute('aria-valuetext', selected ?? '')
  const after = await tooltip.locator('text').first().boundingBox()
  expect(after?.y).toBe(before?.y)
  await expect(demo.getByText('Price to beat', { exact: true })).toHaveCount(1)
  const canvas = demo.locator('canvas').first()
  expect(
    await canvas.evaluate(element => {
      if (!(element instanceof HTMLCanvasElement)) return false
      const context = element.getContext('2d')
      if (!context) return false
      return context
        .getImageData(0, 0, element.width, element.height)
        .data.some((channel, index) => index % 4 === 3 && channel > 0)
    })
  ).toBe(true)
  await page.setViewportSize({ width: 1440, height: 1000 })
  await page.clock.runFor(250)
  await testInfo.attach('Bitcoin rally at 1440px', {
    body: await page.screenshot(),
    contentType: 'image/png',
  })
  await page.clock.resume()
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([])
})

test('demo selection, pause and replay work with reduced motion', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'reduce' })
  await page.goto('/charts')
  const demo = page.getByRole('region', { name: 'Bitcoin chart demos' })
  await demo.getByRole('combobox', { name: 'Demo scenario' }).selectOption('sparse')
  await demo.getByRole('button', { name: 'Pause demo' }).click()
  const slider = demo.getByRole('slider')
  const pausedEnd = await slider.getAttribute('aria-valuemax')
  await expect(demo.getByRole('status')).toHaveText('Demo · Paused')
  await expect(slider).toHaveAttribute('aria-valuemax', pausedEnd ?? '')
  await demo.getByRole('combobox').selectOption('drop')
  await expect(demo.getByRole('combobox')).toHaveValue('drop')
  await expect(page).toHaveURL(/\/charts$/)
  await expect(demo.getByRole('status')).toHaveText('Demo · Playing')
  const before = Number(await slider.getAttribute('aria-valuemax'))
  await expect
    .poll(async () => Number(await slider.getAttribute('aria-valuemax')))
    .toBeGreaterThan(before)
  await demo.getByRole('button', { name: 'Replay demo' }).click()
  await expect(demo.getByRole('status')).toHaveText('Demo · Playing')
})

test('the chart flows through market rollover without losing its hover or history', async ({
  page,
}) => {
  await page.clock.install()
  await page.goto('/charts')
  await page.clock.pauseAt(new Date((await page.evaluate(() => Date.now())) + 1_000))
  const demo = page.getByRole('region', { name: 'Bitcoin chart demos' })
  await expect(demo.getByRole('button', { name: 'Replay demo' })).toBeVisible()
  await demo.getByRole('combobox', { name: 'Demo scenario' }).selectOption('rollover')
  await demo.getByRole('button', { name: 'Replay demo' }).click()
  await page.clock.runFor(250)
  const slider = demo.getByRole('slider')
  const before = Number(await slider.getAttribute('aria-valuemax'))
  await slider.hover({ position: { x: 400, y: 100 } })
  const selected = await slider.getAttribute('aria-valuetext')
  await page.clock.runFor(9_000)
  await expect(slider).toHaveAttribute('aria-valuetext', selected ?? '')
  await expect(demo.getByRole('status')).toHaveText('Demo · Playing')
  expect(Number(await slider.getAttribute('aria-valuemax')) - before).toBeGreaterThan(8_500)
})

test('price flags align with the axis and equal probabilities stay readable', async ({
  page,
}, testInfo) => {
  await page.setViewportSize({ width: 1440, height: 1000 })
  await page.clock.install()
  await page.goto('/charts')
  await page.clock.pauseAt(new Date((await page.evaluate(() => Date.now())) + 1_000))
  const demo = page.getByRole('region', { name: 'Bitcoin chart demos' })
  await demo.getByRole('button', { name: 'Replay demo' }).click()
  await page.clock.runFor(1_000)
  const figure = demo.getByRole('figure')
  const plotBox = await demo.getByRole('slider').boundingBox()
  const figureBox = await figure.boundingBox()
  if (!plotBox || !figureBox) throw new Error('The price plot must be visible')
  expect(plotBox.width / figureBox.width).toBeGreaterThan(0.93)
  const target = figure.getByText('Price to beat', { exact: true })
  const current = figure.getByText(/^\$[\d,]+\.\d{2}$/)
  const targetBox = await target.boundingBox()
  const currentBox = await current.boundingBox()
  expect(targetBox).not.toBeNull()
  expect(currentBox).not.toBeNull()
  if (!targetBox || !currentBox) throw new Error('Price annotations must be visible')
  expect(targetBox.x + targetBox.width).toBeCloseTo(figureBox.x + figureBox.width, 0)
  expect(currentBox.x + currentBox.width).toBeLessThanOrEqual(figureBox.x + figureBox.width)
  expect(currentBox.x).toBeLessThan(plotBox.x + plotBox.width)
  await testInfo.attach('Live price and target at 1440px', {
    body: await page.screenshot(),
    contentType: 'image/png',
  })

  await demo.getByRole('button', { name: 'Replay demo' }).click()
  await demo.getByRole('button', { name: 'Pause demo' }).click()
  await demo.getByRole('radio', { name: 'Market probability chart' }).focus()
  await page.keyboard.press('Space')
  await page.clock.runFor(500)
  const labels = figure.getByText('50%', { exact: true })
  await expect(labels).toHaveCount(3)
  const boxes = await labels.evaluateAll(elements =>
    elements.map(element => {
      const { left, right, top, bottom } = element.getBoundingClientRect()
      return { left, right, top, bottom }
    })
  )
  for (let index = 0; index < boxes.length; index += 1) {
    const currentLabel = boxes[index]
    if (!currentLabel) continue
    for (const other of boxes.slice(index + 1)) {
      const separated =
        currentLabel.right < other.left ||
        other.right < currentLabel.left ||
        currentLabel.bottom < other.top ||
        other.bottom < currentLabel.top
      expect(separated).toBe(true)
    }
  }
  await testInfo.attach('Equal probabilities at 1440px', {
    body: await page.screenshot(),
    contentType: 'image/png',
  })
})

test('historical price stays above its timestamp and clear of the live price', async ({
  page,
}, testInfo) => {
  await page.setViewportSize({ width: 1440, height: 1000 })
  await page.clock.install()
  await page.goto('/charts')
  await page.clock.pauseAt(new Date((await page.evaluate(() => Date.now())) + 1_000))
  const demo = page.getByRole('region', { name: 'Bitcoin chart demos' })
  const figure = demo.getByRole('figure')
  await demo.getByRole('button', { name: 'Replay demo' }).click()
  await page.clock.runFor(250)
  const target = figure.getByText('Price to beat')
  const before = await target.boundingBox()
  await page.clock.runFor(1_000)
  const after = await target.boundingBox()
  if (!before || !after) throw new Error('The target annotation must be present')
  expect(after.y).toBeCloseTo(before.y, 1)

  await demo.getByRole('button', { name: 'Replay demo' }).click()
  await demo.getByRole('button', { name: 'Pause demo' }).click()
  await page.clock.runFor(500)
  const slider = figure.getByRole('slider')
  await slider.focus()
  await page.keyboard.press('End')
  const historical = demo.getByRole('tooltip', { name: 'Historical price' }).getByText('75,674.18')
  await expect(historical).toBeVisible()
  const historicalBox = await historical.boundingBox()
  const figureBox = await figure.boundingBox()
  const plotBox = await slider.boundingBox()
  const currentBox = await figure.getByText(/^\$[\d,]+\.\d{2}$/).boundingBox()
  if (!historicalBox || !figureBox || !plotBox || !currentBox) {
    throw new Error('The plot and price labels must be present')
  }
  expect(historicalBox.y + historicalBox.height).toBeLessThan(plotBox.y)
  expect(historicalBox.y + historicalBox.height).toBeLessThan(currentBox.y)
  for (const edge of ['End', 'Home']) {
    await page.keyboard.press(edge)
    const tooltip = demo.getByRole('tooltip', { name: 'Historical price' })
    const priceBox = await tooltip.getByText(/^75,[\d.]+$/).boundingBox()
    const timeBox = await tooltip.getByText(/^\d{1,2}:\d{2}:\d{2} [AP]M$/).boundingBox()
    if (!priceBox || !timeBox) throw new Error('Both hover labels must remain visible')
    expect(priceBox.y).toBeCloseTo(historicalBox.y, 1)
    expect(priceBox.x + priceBox.width / 2).toBeCloseTo(timeBox.x + timeBox.width / 2, 1)
    expect(priceBox.x).toBeGreaterThanOrEqual(figureBox.x)
    expect(priceBox.x + priceBox.width).toBeLessThanOrEqual(figureBox.x + figureBox.width)
  }
  await testInfo.attach('Historical price above the plot at 1440px', {
    body: await page.screenshot(),
    contentType: 'image/png',
  })
})

for (const clock of [
  { zone: 'Asia/Kolkata', tick: '8:58:30 AM' },
  { zone: 'UTC', tick: '3:28:30 AM' },
]) {
  test.describe(`chart time in ${clock.zone}`, () => {
    test.use({ timezoneId: clock.zone })
    test('formats the same timestamp in the browser timezone', async ({ page }) => {
      await page.clock.install()
      await page.goto('/charts')
      const demo = page.getByRole('region', { name: 'Bitcoin chart demos' })
      await demo.getByRole('button', { name: 'Pause demo' }).click()
      await expect(demo.getByRole('figure').getByText(clock.tick, { exact: true })).toBeVisible()
    })
  })
}
