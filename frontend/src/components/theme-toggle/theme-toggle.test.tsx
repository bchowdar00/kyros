import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { expectNoA11yViolations } from '@kyros/testing/a11y'

import { THEME_COOKIE, parseTheme, themeFor } from './theme'
import { ThemeToggle } from './theme-toggle'

function systemPrefersDark(dark: boolean) {
  vi.stubGlobal(
    'matchMedia',
    vi.fn((query: string) => ({
      addEventListener: vi.fn(),
      matches: dark && query.includes('dark'),
      media: query,
      removeEventListener: vi.fn(),
    }))
  )
}

describe('ThemeToggle', () => {
  beforeEach(() => {
    delete document.documentElement.dataset['theme']
    document.cookie = `${THEME_COOKIE}=; max-age=0; path=/`
  })
  afterEach(() => vi.unstubAllGlobals())

  it('offers the dark theme on a light system and switches to it', async () => {
    systemPrefersDark(false)
    const { container } = render(<ThemeToggle initialTheme={undefined} />)

    await userEvent.click(screen.getByRole('button', { name: 'Switch to the dark theme' }))

    expect(document.documentElement.dataset['theme']).toBe('dark')
    expect(document.cookie).toContain(`${THEME_COOKIE}=dark`)
    expect(screen.getByRole('button', { name: 'Switch to the light theme' })).toBeVisible()
    await expectNoA11yViolations(container)
  })

  it('offers the light theme when the system is dark and nothing was chosen', () => {
    systemPrefersDark(true)
    render(<ThemeToggle initialTheme={undefined} />)

    expect(screen.getByRole('button', { name: 'Switch to the light theme' })).toBeVisible()
  })

  it('lets a stored choice win over the system setting', async () => {
    systemPrefersDark(true)
    render(<ThemeToggle initialTheme="light" />)

    await userEvent.click(screen.getByRole('button', { name: 'Switch to the dark theme' }))

    expect(document.documentElement.dataset['theme']).toBe('dark')
  })
})

describe('parseTheme', () => {
  it('accepts only the two themes', () => {
    expect(parseTheme('dark')).toBe('dark')
    expect(parseTheme('light')).toBe('light')
    expect(parseTheme('sepia')).toBeUndefined()
    expect(parseTheme(undefined)).toBeUndefined()
  })

  it('opens the site dark until the visitor chooses, and keeps a choice once made', () => {
    expect(themeFor(undefined)).toBe('dark')
    expect(themeFor('nonsense')).toBe('dark')
    expect(themeFor('light')).toBe('light')
  })
})
