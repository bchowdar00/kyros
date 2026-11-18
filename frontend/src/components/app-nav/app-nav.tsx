import { Avatar } from '@tiny/ui/components/avatar'
import { TinyMarkIcon } from '@tiny/ui/icons/tiny-mark-icon'
import type { Route } from 'next'
import Link from 'next/link'
import type { ReactNode } from 'react'

export interface AppNavItem {
  href: Route
  label: string
}

export interface AppNavProfile {
  fallback: string
  href: Route
  imageAlt: string
  imageSrc: string
  linkLabel: string
}

export interface AppNavProps {
  balance: string
  balanceLabel: string
  homeHref: Route
  items: readonly AppNavItem[]
  profile: AppNavProfile
  /** Replaces the profile link once a trading account exists. */
  profileMenu?: ReactNode
  searchLabel: string
  searchPlaceholder: string
}

const focusRingClasses =
  'focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring'

export function AppNav({
  balance,
  balanceLabel,
  homeHref,
  items,
  profile,
  profileMenu,
  searchLabel,
  searchPlaceholder,
}: AppNavProps) {
  return (
    <header className="sticky top-0 z-50 h-app-nav shrink-0 border-b border-border-subtle bg-background/95 backdrop-blur-[15px]">
      <nav
        aria-label="Primary"
        className="relative flex h-full items-center justify-between gap-md px-lg lg:px-xl"
      >
        <div className="flex min-w-0 flex-1 items-center lg:w-64 lg:flex-none">
          <Link
            aria-label="Tiny home"
            className={`shrink-0 rounded-xs text-foreground ${focusRingClasses}`}
            href={homeHref}
          >
            <TinyMarkIcon />
          </Link>

          <label className="flex h-4xl min-w-0 flex-1 items-center pl-md focus-within:border-b-2 focus-within:border-focus-ring">
            <span className="sr-only">{searchLabel}</span>
            <input
              aria-label={searchLabel}
              className="h-full min-w-0 flex-1 bg-transparent text-heading-sm font-medium tracking-label text-foreground placeholder:text-muted-foreground focus:outline-none lg:text-label [&::-webkit-search-cancel-button]:appearance-none"
              name="site-search"
              placeholder={searchPlaceholder}
              type="search"
            />
          </label>
        </div>

        {/* Primary destinations appear at the desktop line; a phone menu comes with the shell rebuild. */}
        <div className="absolute left-1/2 hidden -translate-x-1/2 items-center gap-4xl lg:flex">
          {items.map(item => (
            <Link
              className={`rounded-xs text-label font-medium text-foreground transition-colors hover:text-muted-foreground ${focusRingClasses}`}
              href={item.href}
              key={item.href}
            >
              {item.label}
            </Link>
          ))}
        </div>

        <div className="flex shrink-0 items-center justify-end gap-md lg:w-64">
          <span className="inline-flex h-2xl items-center justify-center rounded-xs border border-highlight bg-surface px-md text-label font-medium text-foreground">
            <span className="sr-only">{balanceLabel}</span>
            <span aria-hidden="true" className="font-mono">
              {balance}
            </span>
          </span>

          {profileMenu ?? (
            <Link
              aria-label={profile.linkLabel}
              className={`inline-flex min-h-touch min-w-touch items-center justify-center rounded-pill ${focusRingClasses}`}
              href={profile.href}
            >
              <Avatar
                alt={profile.imageAlt}
                fallback={profile.fallback}
                size="small"
                src={profile.imageSrc}
              />
            </Link>
          )}
        </div>
      </nav>
    </header>
  )
}
