import type { Metadata, Viewport } from 'next'
import { cookies, headers } from 'next/headers'
import Script from 'next/script'
import { connection } from 'next/server'

import { THEME_COOKIE, themeFor } from '@/components/theme-toggle/theme'
import { env } from '@/env'

import { bodyFont, displayFont } from './fonts'
import './globals.css'

export const metadata: Metadata = {
  applicationName: 'Kyros',
  description: 'Markets on what happens next.',
  metadataBase: new URL(env.NEXT_PUBLIC_SITE_URL),
  title: { default: 'Kyros', template: '%s · Kyros' },
}

export const viewport: Viewport = {
  colorScheme: 'light dark',
  initialScale: 1,
  // Mirrors --background, light and dark, in packages/ui/src/styles/tokens.css.
  themeColor: [
    { color: '#ffffff', media: '(prefers-color-scheme: light)' },
    { color: '#07060d', media: '(prefers-color-scheme: dark)' },
  ],
  // Lets fixed bars and sheets extend under the notch and use env(safe-area-inset-*).
  viewportFit: 'cover',
  width: 'device-width',
}

export default async function RootLayout({ children }: LayoutProps<'/'>) {
  // Every route renders per request so the CSP nonce minted in proxy.ts reaches the markup.
  await connection()
  const nonce = (await headers()).get('x-nonce')
  // The visitor's palette choice, else the dark default, applied before any script runs so
  // nothing flashes.
  const theme = themeFor((await cookies()).get(THEME_COOKIE)?.value)

  return (
    <html className={`${bodyFont.variable} ${displayFont.variable}`} data-theme={theme} lang="en">
      <body>
        {children}
        {env.NODE_ENV === 'development' && nonce !== null && nonce.length > 0 ? (
          <Script
            crossOrigin="anonymous"
            nonce={nonce}
            src="https://unpkg.com/react-grab@0.2.0/dist/index.global.js"
            strategy="afterInteractive"
          />
        ) : null}
      </body>
    </html>
  )
}
