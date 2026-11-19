'use client'

import { useState, useSyncExternalStore } from 'react'

import { THEME_COOKIE, type Theme } from './theme'

const ONE_YEAR_SECONDS = 31_536_000
const DARK_QUERY = '(prefers-color-scheme: dark)'

function subscribeToSystem(onChange: () => void) {
  const media = window.matchMedia(DARK_QUERY)
  media.addEventListener('change', onChange)
  return () => media.removeEventListener('change', onChange)
}

/**
 * The palette follows the system until the visitor picks one. The choice is a cookie, not
 * web storage, because the server sets `data-theme` on <html> from it before any script runs.
 */
export function ThemeToggle({ initialTheme }: { initialTheme: Theme | undefined }) {
  const [chosen, setChosen] = useState(initialTheme)
  const systemDark = useSyncExternalStore(
    subscribeToSystem,
    () => window.matchMedia(DARK_QUERY).matches,
    () => false
  )
  const current: Theme = chosen ?? (systemDark ? 'dark' : 'light')
  const next: Theme = current === 'dark' ? 'light' : 'dark'

  return (
    <button
      aria-label={`Switch to the ${next} theme`}
      className="inline-flex h-2xl items-center rounded-sm border-[1.5px] border-foreground px-md text-label font-semibold text-foreground transition-colors hover:bg-foreground hover:text-background focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring"
      onClick={() => {
        document.documentElement.dataset['theme'] = next
        // biome-ignore lint/suspicious/noDocumentCookie: the Cookie Store API is not in every supported browser.
        document.cookie = `${THEME_COOKIE}=${next}; path=/; max-age=${ONE_YEAR_SECONDS}; samesite=lax`
        setChosen(next)
      }}
      type="button"
    >
      <span aria-hidden="true">{next === 'dark' ? 'Dark' : 'Light'}</span>
    </button>
  )
}
