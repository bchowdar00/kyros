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
