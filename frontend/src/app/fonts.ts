import { Bricolage_Grotesque, DM_Sans } from 'next/font/google'

// Both families are open-licensed variable fonts. next/font downloads them at build time and
// serves them from the app origin (CSP `font-src 'self'`), preloaded, with a metric-matched
// fallback so text does not reflow when the file arrives.

/** Body text, labels and every number that updates (tabular figures are set on <body>). */
export const bodyFont = DM_Sans({
  axes: ['opsz'],
  display: 'swap',
  subsets: ['latin'],
  variable: '--font-dm-sans',
})

/** Headings, prices and the wordmark. */
export const displayFont = Bricolage_Grotesque({
  axes: ['opsz'],
  display: 'swap',
  subsets: ['latin'],
  variable: '--font-bricolage',
})
