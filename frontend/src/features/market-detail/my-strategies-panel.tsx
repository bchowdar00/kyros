'use client'

import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { type ReactNode, useState } from 'react'

import { Button } from '@kyros/ui/components/button'
import { ScrollFade } from '@kyros/ui/components/scroll-fade'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@kyros/ui/components/tabs'

import { MyStrategyRow } from './my-strategy-row'
import type { MyStrategiesData, StrategyTab } from './types'

export type MyStrategiesPanelProps = MyStrategiesData & {
  /** Exchange origin for the strategies' on-chain record lines and controls; unset shows neither. */
  apiUrl?: string | undefined
  /** The owning account the pause/stop controls act as; unset signed-out, which hides them. */
  parentAddress?: string | undefined
  openOrders?: ReactNode
  tradeHistory?: ReactNode
}

function StrategyTable({
  apiUrl,
  onDismissStopNotice,
  onStopPending,
  parentAddress,
  rows,
  stopNotices,
}: Pick<MyStrategiesData, 'rows'> & {
  apiUrl: string | undefined
  onDismissStopNotice: (strategyName: string) => void
  onStopPending: (strategyName: string) => void
  parentAddress: string | undefined
  /** Names of strategies whose stop left a position to settle with the round. */
  stopNotices: readonly string[]
}) {
  const [queryClient] = useState(() => new QueryClient())

  return (
    <div className="relative h-[359px] w-full min-w-0 overflow-hidden rounded-md border-[0.5px] border-border-subtle bg-background">
      {/* Inline tables must let scrolling continue to the page at either end. */}
      <section
        aria-label="Scrollable accounts"
        className="h-full w-full overflow-y-auto overscroll-auto focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-focus-ring"
        // Scroll regions must be keyboard reachable (AGENTS.md §4).
        // oxlint-disable-next-line jsx-a11y/no-noninteractive-tabindex
        tabIndex={0}
      >
        {stopNotices.map(name => (
          <div
            className="flex items-start gap-md border-b border-border-subtle px-xl py-md text-label"
            key={name}
          >
            <output className="min-w-0 flex-1 text-muted-foreground">
              {`Stopped ${name}. Its orders are cancelled. Its position could not be closed now, so it settles with the round, and its money returns to your paper balance then.`}
            </output>
            <Button onClick={() => onDismissStopNotice(name)} size="small" variant="ghost">
              Dismiss<span className="sr-only">{`: the notice about ${name}`}</span>
            </Button>
          </div>
        ))}
        <table className="w-full table-fixed border-collapse overflow-hidden">
          <caption className="sr-only">My accounts</caption>
          <colgroup>
            <col />
            <col className="w-[120px]" />
            <col className="w-[60px]" />
            <col className="w-[112px]" />
            <col className="w-[38px]" />
          </colgroup>
          <thead className="sticky top-0 z-10 h-[53px] bg-background">
            <tr className="border-b border-border-subtle">
              <th
                className="overflow-hidden pl-xl text-left text-label font-semibold text-muted-foreground"
                scope="col"
              >
                Account
              </th>
              <th
                className="overflow-hidden whitespace-nowrap pl-sm text-left text-label font-semibold text-muted-foreground"
                scope="col"
              >
                Position
              </th>
              <th
                className="overflow-hidden whitespace-nowrap text-center text-label font-semibold text-muted-foreground"
                scope="col"
              >
                Trades
              </th>
              <th
                className="overflow-hidden whitespace-nowrap text-right text-label font-semibold text-muted-foreground"
                scope="col"
              >
                Market P&amp;L
              </th>
              <th className="pr-xl" scope="col">
                <span className="sr-only">Open account</span>
              </th>
            </tr>
          </thead>
          <QueryClientProvider client={queryClient}>
            <tbody>
              {rows.map((strategy, index) => (
                <MyStrategyRow
                  // The exchange sends the main account first (`accountRows`); it has no record
                  // and no controls, both keyed off an unset `apiUrl`.
                  apiUrl={index === 0 ? undefined : apiUrl}
                  key={strategy.id}
                  onStopPending={onStopPending}
                  parentAddress={parentAddress}
                  showDivider={index === 0}
                  strategy={strategy}
                />
              ))}
            </tbody>
          </QueryClientProvider>
        </table>
      </section>

      <ScrollFade className="h-lg" edge="bottom" />
    </div>
  )
}

function PlaceholderTabPanel({ tab }: { tab: StrategyTab }) {
  return (
    <div
      aria-label={tab.label}
      className="h-[359px] w-full rounded-md border-[0.5px] border-border-subtle bg-background"
    />
  )
}

export function MyStrategiesPanel({
  apiUrl,
  parentAddress,
  rows,
  tabs,
  title,
  openOrders,
  tradeHistory,
}: MyStrategiesPanelProps) {
  const defaultTab = tabs.find(tab => tab.id === 'strategies')?.id ?? tabs[0]?.id
  // Set by a stop that could not close the position. The exchange drops that strategy's row at
  // once while its money returns only when the round settles, so the notice cannot live in the
  // row; it is held here, above the tabs, because a tab's contents unmount when another is shown.
  const [stopNotices, setStopNotices] = useState<readonly string[]>([])

  if (!defaultTab) {
    return null
  }

  return (
    <section aria-label={title} className="w-full min-w-0">
      <Tabs className="flex h-[395px] w-full min-w-0 flex-col gap-xl" defaultValue={defaultTab}>
        <TabsList aria-label={`${title} views`} className="w-full min-w-0 gap-xl overflow-hidden">
          {tabs.map(tab => (
            <TabsTrigger
              className="shrink-0 [font-variant-ligatures:none]"
              key={tab.id}
              value={tab.id}
            >
              {tab.label}
            </TabsTrigger>
          ))}
        </TabsList>

        {tabs.map(tab => (
          <TabsContent className="min-h-0 w-full flex-1" key={tab.id} value={tab.id}>
            {tab.id === 'strategies' ? (
              rows.length > 0 ? (
                <StrategyTable
                  apiUrl={apiUrl}
                  onDismissStopNotice={name =>
                    setStopNotices(names => names.filter(other => other !== name))
                  }
                  onStopPending={name =>
                    setStopNotices(names => (names.includes(name) ? names : [...names, name]))
                  }
                  parentAddress={parentAddress}
                  rows={rows}
                  stopNotices={stopNotices}
                />
              ) : (
                <p className="p-xl text-muted-foreground">Waiting for your accounts.</p>
              )
            ) : tab.id === 'open-orders' && openOrders !== undefined ? (
              openOrders
            ) : tab.id === 'trade-history' && tradeHistory !== undefined ? (
              tradeHistory
            ) : (
              <PlaceholderTabPanel tab={tab} />
            )}
          </TabsContent>
        ))}
      </Tabs>
    </section>
  )
}
