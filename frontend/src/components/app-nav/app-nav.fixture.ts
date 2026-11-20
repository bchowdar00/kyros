import type { AppNavProps } from './app-nav'

export const APP_NAV_FIXTURE = {
  balance: '—',
  balanceLabel: 'Paper balance unknown',
  homeHref: '/',
  items: [
    // Markets is the home screen; there is no separate /markets route.
    { href: '/', label: 'Markets' },
    // Every strategy's sealed record, read from the chain.
    { href: '/records', label: 'Records' },
    // /chat is a design gallery, not a product screen, so it is not a nav destination.
  ],
} as const satisfies AppNavProps
