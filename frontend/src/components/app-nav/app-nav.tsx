import { Badge } from '@kyros/ui/components/badge'
import type { Route } from 'next'
import Link from 'next/link'
import type { ReactNode } from 'react'

import type { Theme } from '@/components/theme-toggle/theme'
import { ThemeToggle } from '@/components/theme-toggle/theme-toggle'
import { Wordmark } from '@/components/wordmark/wordmark'
import type { SiteMode } from '@/features/mode/mode.mapper'

export interface AppNavItem {
  href: Route
  label: string
}

export interface AppNavProps {
  balance: string
  balanceLabel: string
  homeHref: Route
  items: readonly AppNavItem[]
  /** Which money and books the site runs on; no badge when the exchange is not configured. */
  mode?: SiteMode
  /** The account menu, once a trading account exists. */
  profileMenu?: ReactNode
  /** The visitor's stored palette choice; absent means the system setting decides. */
  theme?: Theme | undefined
}

const focusRingClasses =
  'focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring'

/** The masthead: the name, the screens that exist, and what money this is. */
export function AppNav({
  balance,
  balanceLabel,
  homeHref,
  items,
  mode,
  profileMenu,
  theme,
}: AppNavProps) {
  return (
    <header className="sticky top-0 z-50 h-app-nav shrink-0 border-b-2 border-foreground bg-background">
      <nav aria-label="Primary" className="flex h-full items-center gap-2xl px-4xl">
        <Link
          aria-label="Kyros, home"
          className={`shrink-0 rounded-xs ${focusRingClasses}`}
          href={homeHref}
        >
          <Wordmark aria-hidden="true" />
        </Link>

        <div className="flex items-center gap-xl">
          {items.map(item => (
            <Link
              className={`rounded-xs py-xs text-body font-semibold text-muted-foreground transition-colors hover:text-foreground ${focusRingClasses}`}
              href={item.href}
              key={item.label}
            >
              {item.label}
            </Link>
          ))}
        </div>

        <div className="ml-auto flex shrink-0 items-center gap-md">
          {mode === undefined ? null : (
            <Badge
              title={mode.description}
              variant={mode.tone === 'neutral' ? 'positive' : 'warning'}
            >
              {mode.label}
              <span className="sr-only">{mode.description}</span>
            </Badge>
          )}

          <span className="px-xs text-body font-semibold text-foreground">
            <span className="sr-only">{balanceLabel}</span>
            <span aria-hidden="true" className="font-mono">
              {balance}
            </span>
          </span>

          <ThemeToggle initialTheme={theme} />

          {profileMenu}
        </div>
      </nav>
    </header>
  )
}
