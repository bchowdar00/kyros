import type { Metadata, Viewport } from 'next'
import { headers } from 'next/headers'
import Script from 'next/script'
import { connection } from 'next/server'

import { env } from '@/env'

import { roobert, roobertMono } from './fonts'
import './globals.css'

export const metadata: Metadata = {
  applicationName: 'Tiny',
  description: 'Markets on what happens next.',
  metadataBase: new URL(env.NEXT_PUBLIC_SITE_URL),
  title: { default: 'Tiny', template: '%s · Tiny' },
}

export const viewport: Viewport = {
  colorScheme: 'dark',
  initialScale: 1,
  // Mirrors --background in packages/ui/src/styles/tokens.css.
  themeColor: '#000000',
  // Lets fixed bars and sheets extend under the notch and use env(safe-area-inset-*).
  viewportFit: 'cover',
  width: 'device-width',
}

export default async function RootLayout({ children }: LayoutProps<'/'>) {
  // Every route renders per request so the CSP nonce minted in proxy.ts reaches the markup.
  await connection()
  const nonce = (await headers()).get('x-nonce')

  return (
    <html className={`${roobert.variable} ${roobertMono.variable}`} lang="en">
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
