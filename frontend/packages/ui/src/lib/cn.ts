import { type ClassValue, clsx } from 'clsx'
import { extendTailwindMerge } from 'tailwind-merge'

const twMerge = extendTailwindMerge<'tiny-typography'>({
  extend: {
    theme: {
      'font-weight': ['light', 'regular'],
      spacing: [
        'trade-amount',
        'sort-hit',
        'copy',
        'strategy-glow',
        'public-strategy-glow',
        'widget-viewport',
        'strategy-action',
        'xs',
        'sm',
        'md',
        'lg',
        'xl',
        '2xl',
        '3xl',
        '4xl',
        '5xl',
        'avatar-sm',
        'avatar-md',
        'avatar-lg',
        'avatar-overlap',
        'app-nav',
        'below-nav',
        'chart-sm',
        'chart-md',
        'chart-lg',
        'chart-stat-rule',
        'market-card',
        'market-card-chart',
        'signal-trace',
        'signal-value',
        'chat-rail',
        'chat-widget',
        'chat-widget-min',
        'chat-plot',
        'ladder-side',
        'touch',
        'safe-top',
        'safe-right',
        'safe-bottom',
        'safe-left',
      ],
      tracking: ['label'],
      leading: ['prose'],
    },
    classGroups: {
      'tiny-typography': [
        'text-caption',
        'text-label',
        'text-body',
        'text-heading-sm',
        'text-heading-md',
        'text-prose',
        'text-trade-amount',
      ],
    },
  },
})

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}
