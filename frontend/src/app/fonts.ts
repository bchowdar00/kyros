import localFont from 'next/font/local'

// Self-hosted variable fonts served from the app origin (CSP `font-src 'self'`), preloaded,
// with a metric-matched fallback so text does not reflow when the file arrives.
export const roobert = localFont({
  adjustFontFallback: 'Arial',
  display: 'swap',
  src: '../../packages/ui/src/assets/fonts/TinyRoobertProportionalVF.woff2',
  variable: '--font-roobert',
  weight: '300 900',
})

export const roobertMono = localFont({
  adjustFontFallback: false,
  display: 'swap',
  preload: false,
  src: '../../packages/ui/src/assets/fonts/TinyRoobertMonoVF.woff2',
  variable: '--font-roobert-mono',
  weight: '300 900',
})
