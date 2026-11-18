'use client'

import type { Binding } from '../bindings/binding'
import type { ChatData } from '../bindings/chat-data'
import { useCandlesBinding, useChartBinding } from '../bindings/chart'
import { useMarketBinding, usePositionBinding, useStatsBinding } from '../bindings/market'
import {
  useBookBinding,
  useDistributionBinding,
  useFillsBinding,
  useRoundsBinding,
} from '../bindings/tables'
import { FillLog } from '../presets/fill-log'
import { windowPills } from '../resolve/chart'
import { WINDOW_MS_DEFAULT } from '../specs/bound'
import { OrderBook } from '../presets/order-book'
import { PositionCard } from '../presets/position-card'
import { RoundResults } from '../presets/round-results'
import type {
  BookSpec,
  CandlesSpec,
  ChartSpec,
  DistributionSpec,
  FillsSpec,
  MarketSpec,
  PositionSpec,
  RoundsSpec,
  StatsSpec,
} from '../specs'
import { CandleChart } from '../widgets/candle-chart'
import { Card } from '../widgets/card'
import { Histogram } from '../widgets/histogram'
import { StatGrid } from '../widgets/stat-grid'
import { TimeSeries } from '../widgets/time-series'
import {
  type ChartSkeletonShape,
  WidgetFreshness,
  WidgetSkeleton,
  WidgetUnavailable,
} from './frames'

/** Draw a binding's props with `render`, or the frame that says why it cannot be drawn. */
function Bound<Props>({
  label,
  binding,
  skeleton,
  render,
}: {
  label: string
  binding: Binding<Props>
  /** The rows the widget will have, so the skeleton is already its final height. */
  skeleton?: ChartSkeletonShape
  render: (props: Props, status: 'live' | 'stale') => React.ReactNode
}) {
  switch (binding.status) {
    case 'loading':
      return (
        <WidgetSkeleton label={label} {...(skeleton === undefined ? {} : { chart: skeleton })} />
      )
    case 'unavailable':
      return <WidgetUnavailable label={label} reason={binding.reason} />
    default:
      return (
        <WidgetFreshness status={binding.status}>
          {render(binding.props, binding.status)}
        </WidgetFreshness>
      )
  }
}

interface BoundProps<Spec> {
  spec: Spec
  data: ChatData
}

export function ChartWidget({ spec, data }: BoundProps<ChartSpec>) {
  const binding = useChartBinding(spec, data)
  return (
    <Bound
      label={spec.title ?? 'Chart'}
      binding={binding}
      skeleton={{
        title: spec.title,
        legend: spec.refs.length > 1,
        windows: windowPills(spec.window_ms ?? WINDOW_MS_DEFAULT),
      }}
      render={(props, status) => <TimeSeries {...props} live={status === 'live'} />}
    />
  )
}

export function CandlesWidget({ spec, data }: BoundProps<CandlesSpec>) {
  const binding = useCandlesBinding(spec, data)
  return (
    <Bound
      label={spec.title ?? 'BTC candles'}
      binding={binding}
      skeleton={{ title: spec.title ?? 'BTC candles' }}
      render={(props, status) => <CandleChart {...props} live={status === 'live'} />}
    />
  )
}

export function BookWidget({ spec, data }: BoundProps<BookSpec>) {
  const binding = useBookBinding(spec, data)
  return (
    <Bound
      label={spec.title ?? 'Order book'}
      binding={binding}
      render={props => <OrderBook {...props} />}
    />
  )
}

export function MarketWidget({ spec, data }: BoundProps<MarketSpec>) {
  const binding = useMarketBinding(spec, data)
  return (
    <Bound label={spec.title ?? 'Market'} binding={binding} render={props => <Card {...props} />} />
  )
}

export function StatsWidget({ spec, data }: BoundProps<StatsSpec>) {
  const binding = useStatsBinding(spec, data)
  return (
    <Bound
      label={spec.title ?? 'Round stats'}
      binding={binding}
      render={props => <StatGrid {...props} />}
    />
  )
}

export function PositionWidget({ spec, data }: BoundProps<PositionSpec>) {
  const binding = usePositionBinding(spec, data)
  return (
    <Bound
      label={spec.title ?? 'Position'}
      binding={binding}
      render={props => <PositionCard {...props} />}
    />
  )
}

export function RoundsWidget({ spec, data }: BoundProps<RoundsSpec>) {
  const binding = useRoundsBinding(spec, data)
  return (
    <Bound
      label={spec.title ?? 'Round results'}
      binding={binding}
      render={props => <RoundResults {...props} />}
    />
  )
}

export function DistributionWidget({ spec, data }: BoundProps<DistributionSpec>) {
  const binding = useDistributionBinding(spec, data)
  return (
    <Bound
      label={spec.title ?? 'Distribution'}
      binding={binding}
      render={props => <Histogram {...props} />}
    />
  )
}

export function FillsWidget({ spec, data }: BoundProps<FillsSpec>) {
  const binding = useFillsBinding(spec, data)
  return (
    <Bound
      label={spec.title ?? 'Fills'}
      binding={binding}
      render={props => <FillLog {...props} />}
    />
  )
}
