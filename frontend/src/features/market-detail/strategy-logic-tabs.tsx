'use client'

import { Tabs, TabsContent, TabsList, TabsTrigger } from '@kyros/ui/components/tabs'
import { LayoutGroup, motion, useReducedMotion } from 'motion/react'
import { useId, useState } from 'react'

import { CardExpansionAnimation } from '@kyros/ui/lib/card-expansion-animation'

interface StrategyLogicTabsProps {
  logic: string
}

export function StrategyLogicTabs({ logic }: StrategyLogicTabsProps) {
  const [view, setView] = useState('english')
  const layoutId = useId()
  const reducedMotion = useReducedMotion()
  const animation = new CardExpansionAnimation(reducedMotion)

  return (
    <Tabs className="flex flex-col gap-lg" value={view} onValueChange={setView}>
      <div className="flex items-center justify-between gap-md">
        <h3 className="text-heading-sm font-regular">The Logic</h3>
        <LayoutGroup id={layoutId}>
          <TabsList
            aria-label="Strategy logic view"
            className="gap-0 rounded-sm border border-highlight bg-surface-raised p-0.5"
          >
            {['English', 'Nodes'].map(label => (
              <TabsTrigger
                key={label}
                value={label.toLowerCase()}
                className="relative h-2xl cursor-pointer px-md text-label font-medium data-[state=active]:text-background after:absolute after:inset-x-0 after:top-1/2 after:min-h-touch after:-translate-y-1/2 after:content-['']"
              >
                {view === label.toLowerCase() ? (
                  <motion.span
                    aria-hidden="true"
                    className="absolute inset-0 rounded-xs border border-foreground bg-foreground/80"
                    layoutId="active-logic-view"
                    transition={animation.transition}
                  />
                ) : null}
                <span className="relative">{label}</span>
              </TabsTrigger>
            ))}
          </TabsList>
        </LayoutGroup>
      </div>
      <div className="grid rounded-sm border border-highlight">
        <TabsContent
          forceMount
          aria-hidden={view !== 'english'}
          inert={view !== 'english'}
          value="english"
          className="col-start-1 row-start-1 px-xl py-lg data-[state=inactive]:invisible"
        >
          <p className="text-heading-sm font-regular leading-normal">{logic}</p>
        </TabsContent>
        <TabsContent value="nodes" className="col-start-1 row-start-1 min-h-0" />
      </div>
    </Tabs>
  )
}
