'use client'

import { CodeBlock } from '../widgets/code-block'
import { Diff } from '../widgets/diff'
import { MarkdownBlock } from '../widgets/markdown-block'

export function DiffExample() {
  return (
    <Diff
      label="Strategy edit"
      title="What changed"
      from="Mean reversion v1"
      to="Mean reversion v2"
      lines={[
        { kind: 'context', text: 'If either side drops 12¢ or more' },
        { kind: 'removed', text: 'within 3 minutes, buy it.' },
        { kind: 'added', text: 'within 3 minutes while the score' },
        { kind: 'added', text: 'is within 6 points, buy it.' },
        { kind: 'context', text: 'Sell at +6¢ or hold to settlement.' },
        { kind: 'added', text: 'Cancel if RSI crosses back above 50.' },
      ]}
    />
  )
}
import { Sources } from '../widgets/sources'

export function SourcesExample() {
  return (
    <Sources
      label="Sources for the price answer"
      title="Sources"
      meta="3 read"
      sources={[
        {
          id: 'pyth',
          title: 'BTC/USD price feed, Pyth Lazer real-time channel',
          url: 'https://docs.pyth.network/lazer',
          retrieved: '2:33 PM',
          excerpt: 'Aggregate of publisher quotes with a confidence interval, updated sub-second.',
        },
        {
          id: 'coinbase',
          title: 'Coinbase spot ticker BTC-USD',
          url: 'https://www.coinbase.com/price/bitcoin',
          retrieved: '2:33 PM',
        },
        {
          id: 'series',
          title: 'Tiny series table, btc-2m round rules',
          url: 'https://tiny.xyz/markets/btc-2m',
          retrieved: '2:30 PM',
          excerpt: 'Strike is the oracle price at open; close at or above resolves YES.',
        },
      ]}
    />
  )
}

const COMPILED_STRATEGY = `{
  "series": "btc-2m",
  "trigger": { "signal": "ema_cross", "op": ">", "value": 0 },
  "conditions": [
    { "signal": "rsi_14", "op": "<", "value": 40 },
    { "signal": "dist_strike", "op": ">", "value": 0 }
  ],
  "action": { "side": "YES", "limit_ticks": 580, "size": 100, "post_only": true },
  "exit": { "take_profit_ticks": 60, "hold_to_settlement": true }
}`

export function CodeExample() {
  return (
    <CodeBlock
      label="Compiled strategy"
      title="Compiled program"
      language="json"
      code={COMPILED_STRATEGY}
    />
  )
}

const STRATEGY_NOTES = `## Why the entry fired

The **9/21 EMA cross** flipped bullish at 2:31 PM while RSI was still under 40, which is the
setup this strategy waits for.

- Spot was \`$18\` above the strike with 47 seconds left
- The YES ask sat at 58¢ against a fair value of 61¢
- Depth at the touch was thin, so the order went in as a *post-only* limit

1. Buy YES at 58¢ or better
2. Sell at +6¢ or hold to settlement
3. Cancel if the cross reverses before the fill`

export function MarkdownExample() {
  return (
    <MarkdownBlock
      label="Strategy notes"
      title="Notes"
      meta="From the strategy"
      source={STRATEGY_NOTES}
    />
  )
}
