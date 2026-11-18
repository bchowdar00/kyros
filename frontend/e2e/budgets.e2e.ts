import { expect, type Page, test } from '@playwright/test'

import { gatedRoutes } from './routes'

const KB = 1024
/** Time for late chunks and prefetches to land after `load`. */
const SETTLE_MS = 1_500

/**
 * Bytes as they cross the wire: encoded (gzip) sizes reported by the browser, summed per
 * resource type. This is what a phone on 4G downloads before the page is usable.
 */
async function measure(page: Page, path: string) {
  let documentBytes = 0
  let scriptBytes = 0
  const scripts: string[] = []
  const pending: Promise<void>[] = []

  page.on('response', response => {
    const request = response.request()
    const type = request.resourceType()

    if (response.status() !== 200 || (type !== 'document' && type !== 'script')) {
      return
    }

    pending.push(
      request.sizes().then(({ responseBodySize }) => {
        if (type === 'document') {
          documentBytes += responseBodySize
        } else {
          scriptBytes += responseBodySize
          scripts.push(
            `${new URL(request.url()).pathname} ${(responseBodySize / KB).toFixed(1)} KB`
          )
        }
      })
    )
  })

  await page.goto(path, { waitUntil: 'load' })
  await page.waitForTimeout(SETTLE_MS)
  await Promise.all(pending)

  return { documentBytes, scriptBytes, scripts }
}

for (const route of gatedRoutes) {
  test.describe(route.name, () => {
    test('first-load JavaScript is within budget', async ({ page }, testInfo) => {
      const { scriptBytes, scripts } = await measure(page, route.path)
      const total = `${(scriptBytes / KB).toFixed(1)} KB gzip`
      testInfo.annotations.push({ type: 'js-on-the-wire', description: total })

      expect(scriptBytes, `JS on the wire (${total}):\n${scripts.join('\n')}`).toBeLessThanOrEqual(
        route.budget.jsKb * KB
      )
    })

    test('HTML document is within budget', async ({ page }, testInfo) => {
      const { documentBytes } = await measure(page, route.path)
      const total = `${(documentBytes / KB).toFixed(1)} KB gzip`
      testInfo.annotations.push({ type: 'html-on-the-wire', description: total })

      expect(documentBytes).toBeLessThanOrEqual(route.budget.htmlKb * KB)
    })
  })
}
