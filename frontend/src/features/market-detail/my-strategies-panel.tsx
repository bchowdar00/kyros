'use client'

import type { ReactNode } from 'react'

import { ScrollFade } from '@tiny/ui/components/scroll-fade'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@tiny/ui/components/tabs'

import { MyStrategyRow } from './my-strategy-row'
import type { MyStrategiesData, StrategyTab } from './types'

export type MyStrategiesPanelProps = MyStrategiesData & {
  openOrders?: ReactNode
  tradeHistory?: ReactNode
}

function StrategyTable({ rows }: Pick<MyStrategiesData, 'rows'>) {
  return (
    <div className="relative h-[359px] w-full min-w-0 overflow-hidden rounded-md border-[0.5px] border-border-subtle bg-surface-subtle">
      {/* Inline tables must let scrolling continue to the page at either end. */}
      <section
        aria-label="Scrollable accounts"
        className="h-full w-full overflow-y-auto overscroll-auto focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-focus-ring"
        // Scroll regions must be keyboard reachable (AGENTS.md §4).
        // oxlint-disable-next-line jsx-a11y/no-noninteractive-tabindex
        tabIndex={0}
      >
        <table className="w-full table-fixed border-collapse overflow-hidden">
          <caption className="sr-only">My accounts</caption>
          <colgroup>
            <col />
            <col className="w-[120px]" />
            <col className="w-[60px]" />
            <col className="w-[112px]" />
            <col className="w-[38px]" />
          </colgroup>
          <thead className="sticky top-0 z-10 h-[53px] bg-background/80 backdrop-blur-glass">
            <tr className="border-b border-border-subtle">
              <th
                className="overflow-hidden pl-xl text-left text-caption font-semibold uppercase text-muted-foreground"
                scope="col"
              >
                Account
              </th>
              <th
                className="overflow-hidden whitespace-nowrap pl-sm text-left text-caption font-semibold uppercase text-muted-foreground"
                scope="col"
              >
                Position
              </th>
              <th
                className="overflow-hidden whitespace-nowrap text-center text-caption font-semibold uppercase text-muted-foreground"
                scope="col"
              >
                Trades
              </th>
              <th
                className="overflow-hidden whitespace-nowrap text-right text-caption font-semibold uppercase text-muted-foreground"
                scope="col"
              >
                Market P&amp;L
              </th>
              <th className="pr-xl" scope="col">
                <span className="sr-only">Open account</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {rows.map((strategy, index) => (
              <MyStrategyRow key={strategy.id} showDivider={index === 0} strategy={strategy} />
            ))}
          </tbody>
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
      className="h-[359px] w-full rounded-md border-[0.5px] border-border-subtle bg-surface-subtle"
    />
  )
}

export function MyStrategiesPanel({
  rows,
  tabs,
  title,
  openOrders,
  tradeHistory,
}: MyStrategiesPanelProps) {
  const defaultTab = tabs.find(tab => tab.id === 'strategies')?.id ?? tabs[0]?.id

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
                <StrategyTable rows={rows} />
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
