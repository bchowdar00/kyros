export type Theme = 'dark' | 'light'

/**
 * The visitor's palette choice. A cookie rather than web storage, because the root layout
 * reads it on the server and sets `data-theme` on <html> before any script runs. This module
 * has no `'use client'` so the server can call it.
 */
export const THEME_COOKIE = 'kyros-theme'

export function parseTheme(value: string | undefined): Theme | undefined {
  return value === 'dark' || value === 'light' ? value : undefined
}

/** The palette a visitor sees before they have chosen: the site is designed dark first. */
const DEFAULT_THEME: Theme = 'dark'

/** The palette to render for a cookie value: the visitor's choice, else the default. */
export function themeFor(value: string | undefined): Theme {
  return parseTheme(value) ?? DEFAULT_THEME
}
