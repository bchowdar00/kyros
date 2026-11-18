'use client'

import { Card } from '../widgets/card'
import { Heatmap } from '../widgets/heatmap'

export function CardExample() {
  return (
    <Card
      label="Mean reversion strategy summary"
      eyebrow="Mean reversion v2"
      eyebrowMeta={
        <span>
          Running <span className="font-mono">14m</span>
        </span>
      }
      hero={{ label: 'Unrealised', value: '+$42.10', direction: 'up', delta: '+16.8%' }}
      stats={[
        { id: 'deployed', label: 'Deployed', value: '$250.00' },
        { id: 'fills', label: 'Fills', value: '12' },
        { id: 'win', label: 'Win rate', value: '58%' },
        { id: 'drawdown', label: 'Max drawdown', value: '−$18.40' },
        { id: 'edge', label: 'Avg edge at entry', value: '+2.9¢' },
        { id: 'rounds', label: 'Rounds', value: '7' },
      ]}
      footer="Buys the side that dropped 12¢ or more inside 3 minutes. Sells at +6¢ or holds to settlement."
    />
  )
}

const REGIMES = ['Chop', 'Trend', 'Squeeze']
const REMAINING = ['0–30s', '30–60s', '60–90s', '90–120s']
/** Average edge in cents by regime and time remaining; the alpha lives late in chop. */
const EDGE_BY_REGIME = [
  [3.4, 2.1, 0.8, -0.3],
  [-1.2, 0.4, 1.9, 2.6],
  [0.2, -0.9, -2.4, -1.1],
]

export function HeatmapExample() {
  return (
    <Heatmap
      label="Average edge by regime and time remaining"
      title="Edge by regime"
      meta="Last 400 rounds"
      rows={REGIMES}
      rowHeader="Regime"
      columns={REMAINING}
      values={EDGE_BY_REGIME}
      format={value => `${value > 0 ? '+' : value < 0 ? '−' : ''}${Math.abs(value).toFixed(1)}¢`}
      diverging
    />
  )
}
