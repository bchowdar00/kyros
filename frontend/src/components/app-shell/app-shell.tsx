import { cookies } from 'next/headers'
import type { ReactNode } from 'react'

import { APP_NAV_FIXTURE } from '@/components/app-nav/app-nav.fixture'
import { LiveAppNav } from '@/components/app-nav/live-app-nav'
import { MobileComingSoon } from '@/components/mobile-coming-soon/mobile-coming-soon'
import { THEME_COOKIE, parseTheme } from '@/components/theme-toggle/theme'
import { env } from '@/env'
import { IdentityGate } from '@/features/identity/identity-gate'
import { loadSiteMode } from '@/server/mode.server'

interface AppShellProps {
  children: ReactNode
}

/**
 * Nav plus content area, shared by the app segment and the root 404 so a dead link
 * keeps the shell instead of dropping onto a bare page. With an exchange configured the
 * whole shell waits behind the identity gate until the browser holds a trading account.
 *
 * Desktop-first interim (AGENTS.md §0.2): below `lg` the coming-soon screen replaces the
 * app. CSS decides the branch; `display: none` keeps the hidden one out of the
 * accessibility tree and focus order.
 */
export async function AppShell({ children }: AppShellProps) {
  const mode = await loadSiteMode()
  const theme = parseTheme((await cookies()).get(THEME_COOKIE)?.value)

  return (
    <div className="bg-background text-foreground">
      <div className="lg:hidden">
        <MobileComingSoon />
      </div>
      <div className="hidden min-h-dvh flex-col lg:flex">
        <IdentityGate
          {...(env.NEXT_PUBLIC_API_URL === undefined ? {} : { apiUrl: env.NEXT_PUBLIC_API_URL })}
        >
          <LiveAppNav
            {...APP_NAV_FIXTURE}
            theme={theme}
            {...(mode === undefined ? {} : { mode })}
          />
          <div className="min-h-below-nav flex-1">{children}</div>
        </IdentityGate>
      </div>
    </div>
  )
}
