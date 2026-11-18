'use client'

import * as TabsPrimitive from 'radix-ui/tabs'
import type { ComponentPropsWithRef } from 'react'

import { cn } from '../lib/cn'

export type TabsProps = ComponentPropsWithRef<typeof TabsPrimitive.Root>

export function Tabs(props: TabsProps) {
  return <TabsPrimitive.Root {...props} />
}

export type TabsListProps = ComponentPropsWithRef<typeof TabsPrimitive.List> & {
  'aria-label': string
}

export function TabsList({ className, ...props }: TabsListProps) {
  return <TabsPrimitive.List {...props} className={cn('flex items-center gap-xl', className)} />
}

export type TabsTriggerProps = ComponentPropsWithRef<typeof TabsPrimitive.Trigger>

export function TabsTrigger({ className, ...props }: TabsTriggerProps) {
  return (
    <TabsPrimitive.Trigger
      {...props}
      className={cn(
        'rounded-xs text-heading-sm font-regular text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring disabled:opacity-50 data-[state=active]:text-foreground',
        className
      )}
    />
  )
}

export type TabsContentProps = ComponentPropsWithRef<typeof TabsPrimitive.Content>

export function TabsContent({ className, ...props }: TabsContentProps) {
  return (
    <TabsPrimitive.Content
      {...props}
      className={cn(
        'focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus-ring',
        className
      )}
    />
  )
}
