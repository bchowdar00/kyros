import { plannedRoute } from '@/lib/routes'

import type { AppNavProps } from './app-nav'

export const APP_NAV_FIXTURE = {
  balance: '$2,940.44',
  balanceLabel: 'Portfolio balance: $2,940.44',
  homeHref: '/',
  items: [
    // Markets is the home screen; there is no separate /markets route to plan for.
    { href: '/', label: 'Markets' },
    { href: plannedRoute('/strategies'), label: 'Strategies' },
    { href: plannedRoute('/leaderboard'), label: 'Leaderboard' },
    { href: plannedRoute('/rewards'), label: 'Rewards' },
    { href: '/charts', label: 'Charts' },
    { href: '/chat', label: 'Chat' },
  ],
  profile: {
    fallback: 'M',
    href: plannedRoute('/profile'),
    imageAlt: 'Mohamed',
    imageSrc: '/assets/market/avatars/profile.png',
    linkLabel: 'Open Mohamed profile',
  },
  searchLabel: 'Search Tiny',
  searchPlaceholder: 'Search Tiny...',
} as const satisfies AppNavProps
