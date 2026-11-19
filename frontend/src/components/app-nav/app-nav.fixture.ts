import type { AppNavProps } from './app-nav'

export const APP_NAV_FIXTURE = {
  balance: '—',
  balanceLabel: 'Paper balance unknown',
  homeHref: '/',
  items: [
    // Markets is the home screen; there is no separate /markets route.
    { href: '/', label: 'Markets' },
    { href: '/chat', label: 'Chat' },
  ],
} as const satisfies AppNavProps
