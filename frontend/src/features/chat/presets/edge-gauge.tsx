import { Gauge } from '../widgets/gauge'

export interface EdgeGaugeProps {
  series: string
  fairCents: number
  marketCents: number
}

/** Edge beyond which the spread and slippage are covered; inside it the read is "hold". */
const ACTIONABLE_CENTS = 3
const RANGE_CENTS = 10

function signedCents(value: number) {
  return `${value > 0 ? '+' : value < 0 ? '−' : ''}${Math.abs(value)}¢`
}

/** Fair minus market as a bounded scalar with sell, hold and buy zones. */
export function EdgeGauge({ series, fairCents, marketCents }: EdgeGaugeProps) {
  const edge = fairCents - marketCents
  const reading =
    edge >= ACTIONABLE_CENTS
      ? `YES is underpriced by ${edge}¢ against fair value.`
      : edge <= -ACTIONABLE_CENTS
        ? `YES is overpriced by ${Math.abs(edge)}¢ against fair value.`
        : 'Inside the spread. Nothing to do at this price.'
  return (
    <Gauge
      label={`${series} edge`}
      title="Edge"
      meta={
        <span className="font-mono">
          fair {fairCents}¢, market {marketCents}¢
        </span>
      }
      value={Math.max(-RANGE_CENTS, Math.min(RANGE_CENTS, edge))}
      min={-RANGE_CENTS}
      max={RANGE_CENTS}
      format={signedCents}
      zones={[
        { from: -RANGE_CENTS, to: -ACTIONABLE_CENTS, tone: 'negative', label: 'Sell YES' },
        { from: -ACTIONABLE_CENTS, to: ACTIONABLE_CENTS, tone: 'neutral', label: 'Hold' },
        { from: ACTIONABLE_CENTS, to: RANGE_CENTS, tone: 'positive', label: 'Buy YES' },
      ]}
      reading={reading}
    />
  )
}
