import { cents, createMoneyFormatter } from '@tiny/money'

import { Card } from '../widgets/card'

export interface MarketCardProps {
  /** Series name as the eyebrow, for example `BTC 2m`. */
  series: string
  /** Seconds until the round resolves. */
  secondsRemaining: number
  spotCents: number
  strikeCents: number
  yesCents: number
  spreadCents: number
  fairCents: number
  marketCents: number
}

const money = createMoneyFormatter('en-US')

function countdown(seconds: number) {
  const whole = Math.max(0, Math.floor(seconds))
  return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, '0')}`
}

function signedCents(value: number) {
  return `${value > 0 ? '+' : value < 0 ? '−' : ''}${Math.abs(value)}¢`
}

/** The whole round at a glance: what the agent shows for "tell me about the BTC 2m". */
export function MarketCard({
  series,
  secondsRemaining,
  spotCents,
  strikeCents,
  yesCents,
  spreadCents,
  fairCents,
  marketCents,
}: MarketCardProps) {
  const distance = spotCents - strikeCents
  const edge = fairCents - marketCents
  return (
    <Card
      label={`${series} market`}
      eyebrow={`${series} binary`}
      eyebrowMeta={<span className="font-mono">T−{countdown(secondsRemaining)}</span>}
      hero={{
        value: money.usd(cents(spotCents)),
        delta: money.usd(cents(distance), { signDisplay: 'always' }),
      }}
      stats={[
        { id: 'strike', label: 'Strike', value: money.usd(cents(strikeCents)) },
        { id: 'yes', label: 'YES', value: `${yesCents}¢` },
        { id: 'spread', label: 'Spread', value: `${spreadCents}¢` },
        { id: 'fair', label: 'Fair', value: `${fairCents}¢` },
        { id: 'market', label: 'Market', value: `${marketCents}¢` },
        {
          id: 'edge',
          label: 'Edge',
          value: signedCents(edge),
          ...(edge > 0
            ? { direction: 'up' as const }
            : edge < 0
              ? { direction: 'down' as const }
              : {}),
        },
      ]}
    />
  )
}
