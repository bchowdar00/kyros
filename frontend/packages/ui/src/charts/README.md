# Charts

Canvas line charts with SVG axes, overlays and candle marks for Tiny. No charting library: scales, ticks, paths,
downsampling and candle aggregation are implemented in `lib/` and rendered by
small React layers in `internal/`.

## Usage

```tsx
import { MarketChart } from '@tiny/ui/charts/market-chart'
import { ReferenceLine, TimeMarker } from '@tiny/ui/charts/chart-overlays'

<div className="h-[292px]">
  <MarketChart
    kind="probability"            // 'probability' | 'price' | 'candlestick'
    series={[{ id: 'yes', label: 'Yes', color: 'var(--positive)', points }]}
    range={{ start, end }}        // visible window (epoch ms); defaults to the data extent
    timeZone="America/New_York"
    live                          // pulse the latest sample
    area                          // wash under the line; on for price charts
    onHoverChange={setHover}      // scrub callback for headlines and legends
    overlays={<ValueFlag color="var(--asset-btc)" value={latestPrice} />}
  >
    <TimeMarker label="Tip-off" time={tipOff} />
  </MarketChart>
</div>
```

`points` are `{ time: number /* epoch ms */, value: number }`. Probability
values are on a 0–100 scale. Switching `kind` re-renders the same series:

- `probability`: fixed 0–100 axis, glowing lines, large end-of-line labels.
- `price`: auto or target-centred axis (`target` draws the labelled reference
  line and the domain becomes `target ± n` nice steps), currency formatting.
- `candlestick`: OHLC candles aggregated from the first series (or pass
  `candles`), `candleInterval` in ms, hover readout of open/high/low/close.

The chart measures its container with a `ResizeObserver`; pass `width` and
`height` for a fixed size (tests do this).

## Interaction

- Pointer and touch scrub snap to the nearest sample. The stroke keeps its
  colour while scrubbing; the crosshair line and the dot on the line mark
  the cursor.
- The hit area is a `role="slider"`: arrow keys step samples, Shift steps 5%,
  Home/End jump, Escape clears. Keyboard scrubbing is announced through a
  polite live region.
- `onHoverChange` receives `{ time, values[], candle? }` or `null`.

## Overlays

`children` render inside the plot's SVG group after the series; `overlays`
render after the axes and outside the plot clip, which is where an axis-gutter
mark such as `ValueFlag` (`@tiny/ui/charts/value-flag`, the live price label)
belongs. Use
`useChartGeometry()` from `@tiny/ui/charts/chart-overlays` to read `xScale`,
`yScale`, `plot` and formatting and draw anything in pixel space:

```tsx
function FillMarker({ time, value }: { time: number; value: number }) {
  const { xScale, yScale } = useChartGeometry()
  return <circle cx={xScale(time)} cy={yScale(value)} r={3} className="fill-action" />
}
```

## Live data

`useLiveChartWindow` advances only the visible time range. `PriceMarketCard`
receives observations from the application's feed and never generates ticks.
The live range scrolls continuously at wall-clock speed; replay sources can
advance from sample timestamps. Reduced motion uses a one-second clock update.
Simulation state and generators are mounted only by the `/charts` playground.

## Market cards

Two composed cards own everything above the plot, so a market looks the same
wherever it is rendered — the market screen and /charts import the same file:

- `PriceMarketCard` (`@tiny/ui/charts/price-market-card`): a market that
  resolves on a price against a target. Headline, compact asset-price / probability
  toggle, live price flag, strike line. Given `round`, the run-up before the
  open draws dimmed behind an "Open" marker (`MarketChart`'s `dimBefore`), so a
  new round starts with the move that led into it instead of an empty plot.
  The line ends 12% short of the axis gutter (`MarketChart`'s `trailingSpace`),
  so the tip and its value flag sit apart; ticks continue into the gap.
- `ProbabilityMarketCard` (`@tiny/ui/charts/probability-market-card`): a
  two-outcome market over a scheduled window. Both live probabilities and period markers. The second outcome is the first one's complement,
  because two outcomes always sum to 100.

Both render supplied observations. `chartClassName` sets the plot height (`h-chart-lg` on a market screen,
`h-chart-md` on a card).

## Price headline

`PriceHeadline` (`@tiny/ui/charts/price-headline`) is the four-stat block
above a price chart: target, current price, change, and time left. The
market screen and /charts render the same component, so the two cannot drift.
The app owns the market clock and passes `timeLeft`, or `PriceMarketCard` can
derive it from `marketEndsAt`. Prices display cents, and change is calculated
from the same integer cent amounts. Digit animations share a 180ms transition;
signs and currency symbols stay outside the reels. Degraded feeds stay visibly
labelled beside the current delta.

## Data helpers (`@tiny/ui/charts/lib/*`)

- `candles`: `aggregateCandles(points, intervalMs, range?)`, `CANDLE_INTERVALS`.
- `stroke`: the drawn form of a line. `extendStroke` keeps an append-only
  `Stroke`: an even, epoch-anchored 500 ms grid where each bucket commits once
  (the value carried forward at that instant, blended into a causal EMA) and is
  never rewritten. Anything but an append rebuilds. `useStrokeSeries` in
  `internal/` holds one stroke per series id across renders.
- `downsample`: `downsampleM4` (first/min/max/last per bucket), `resampleMean`
  and `smoothPoints` remain utilities; the stroke no longer uses them.
- `scale`: linear scales, nice ticks, `centeredDomain`, `findNearestIndex`.
- `time`: wall-clock aligned `timeTicks`, `formatTimeTick`, `formatTooltipTime`.
- `format`: `formatValue`, `formatSignedValue` for percent, currency, number.
- `mock`: seeded `createMockProbabilityPoints`, `createMockPricePoints`.

## Design notes

Line strokes are drawn from a committed grid (`lib/stroke`): 500 ms buckets of
carried-forward observations under a light causal EMA, drawn through a monotone
cubic curve that cannot overshoot a real high or low. A bucket is committed once
a newer sample exists past it and is then final, so earlier points never move;
only the newest bucket springs into place. The endpoint mark and value flag sit
on the stroke's last point, while their text, hover and headline values read the
observed samples. A live time window advances both edges at clock speed; historical
window switches and value-domain changes animate. A kind switch snaps both.

Colours come only from semantic tokens: series colours are passed as
`var(--token)` strings, everything else uses Tailwind token utilities. SVG glow
is limited to endpoint marks and text, never data paths. Grid rows, axis labels and the y domain
are derived from the same tick set so they can never disagree.

## Plug and play

The same `MarketChart` element serves every market type; only the props change.

```tsx
// Sports / binary market
<MarketChart kind="probability" series={[yes, no]} range={gameWindow} live />

// Crypto "above target" market
<MarketChart kind="price" series={[btc]} target={75_674.18} range={window} />

// Same market as candles
<MarketChart kind="candlestick" series={[btc]} target={75_674.18} candleInterval={60_000} />

// Pre-aggregated candles from an API
<MarketChart kind="candlestick" candles={ohlc} series={[]} valueFormat={USD_FORMAT} />
```

`CHART_KINDS` exports the kind list for building toggles. Colours are CSS
strings so any semantic token works; value formatting is data
(`ValueFormat`) so props stay serialisable across the server boundary.

## Edge cases the component handles

- Empty `series` or `candles`: axes render with a muted "No data yet" label.
- A single sample: the endpoint dot and label render, no path is drawn.
- Non-finite values are skipped for both the path and the y domain.
- Samples outside `range` are sliced away before downsampling and clipped to
  the plot; the endpoint never leaves the plot.
- `range` narrower or wider than the data, and a `range` whose start equals
  its end.
- Series with different lengths or time grids: the crosshair snaps per series
  by time, and candles are looked up by time, so live appends and kind
  switches never index a stale sample.
- Switching `kind` clears the crosshair and snaps the y domain; ordinary data
  updates animate it.
- Live appends and rolling-buffer updates glide with a velocity-preserving
  spring; resets and new series identities snap.
- Fractional device pixels and container resizes: the chart re-measures and
  redraws in pixel space, never stretching text.
- Time zones and locales are explicit props so server and client render the
  same labels; Intl's narrow no-break spaces are normalised.

## Live chart behavior

Price and probability strokes render on canvas without SVG filters. The stroke
is an append-only grid of carried-forward observations under a light causal
EMA, so bursts of ticks and quiet gaps read as one fluid line and history stays
fixed as new ticks arrive; hover and headline values use the observed samples.

Live scales include the target, reserve a minimum range for quiet prices, and
retain their bounds through ordinary ticks. A large move expands the scale; a
substantially smaller visible range lets it contract. Hover selection uses the
observed timestamp, surviving buffer trimming, with separate fixed-height price
and time readouts. The current-price flag shares the animated tip coordinate. The target label sits
above its guide on an opaque background, preserving the full plot width.
Probability endpoint labels use a separate column from the axis ticks.

The application owns feed lifetime by origin and symbol, independently of market
IDs and deadlines. The chart component is keyed to the series, so a market
rollover updates the target and countdown while its scale, hover, connection and
bounded observed history continue. A brief unmount during navigation
gets a 30-second grace period before an unused connection is released.

The market loader requests recorded price history for the trailing window and
uses it only outside the interval already covered by live observations.
Refreshed sampling grids never interleave with raw ticks. Missing history stays empty;
observations outside the backend's reported coverage are discarded.

`/charts` runs the same price card with an explicit sample clock. Demo
choices include rally, drop, recovery, sparse history, disconnect and rollover.
The scenario selector stays local to the gallery; it does not use URL parameters.
They are deterministic simulations and are labeled as such. Probability and
candlestick comparisons load when requested.
