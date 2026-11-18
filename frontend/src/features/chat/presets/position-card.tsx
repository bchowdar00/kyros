import { cents, createMoneyFormatter } from '@tiny/money'

import { Card } from '../widgets/card'

export interface PositionCardProps {
  series: string
  /** Signed contracts: positive is YES, negative is NO. */
  contracts: number
  /** Average entry price per contract, in cents. */
  entryCents: number
  /** The current mid for the side held, in cents. */
  markCents: number
  lockedCents: number
  /** Seconds until the round resolves. */
  secondsRemaining: number
}

const money = createMoneyFormatter('en-US')
/** One contract pays $1 at resolution. */
const PAYOUT_CENTS = 100

function countdown(seconds: number) {
  const whole = Math.max(0, Math.floor(seconds))
  return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, '0')}`
}

/** What the user holds in this round and what it is worth under each resolution. */
export function PositionCard({
  series,
  contracts,
  entryCents,
  markCents,
  lockedCents,
  secondsRemaining,
}: PositionCardProps) {
  const side = contracts >= 0 ? 'YES' : 'NO'
  const size = Math.abs(contracts)
  const cost = size * entryCents
  const unrealised = size * (markCents - entryCents)
  const ifWin = size * PAYOUT_CENTS - cost
  const ifLose = -cost
  const signed = (value: number) => money.usd(cents(value), { signDisplay: 'always' })

  return (
    <Card
      label={`${series} position`}
      eyebrow={`Position in ${series}`}
      eyebrowMeta={<span className="font-mono">T−{countdown(secondsRemaining)}</span>}
      hero={{
        label: `${size} ${side} at ${entryCents}¢`,
        value: signed(unrealised),
        direction: unrealised >= 0 ? 'up' : 'down',
        delta: `mark ${markCents}¢`,
      }}
      stats={[
        { id: 'if-yes', label: 'If YES', value: signed(side === 'YES' ? ifWin : ifLose) },
        { id: 'if-no', label: 'If NO', value: signed(side === 'NO' ? ifWin : ifLose) },
        { id: 'locked', label: 'Locked', value: money.usd(cents(lockedCents)) },
      ]}
    />
  )
}
