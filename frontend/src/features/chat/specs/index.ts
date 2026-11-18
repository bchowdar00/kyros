import * as z from 'zod/mini'

import {
  BookSpecSchema,
  CandlesSpecSchema,
  ChartSpecSchema,
  DistributionSpecSchema,
  FillsSpecSchema,
  MarketSpecSchema,
  PositionSpecSchema,
  RoundsSpecSchema,
  StatsSpecSchema,
} from './bound'
import {
  CodeSpecSchema,
  DiffSpecSchema,
  MarkdownSpecSchema,
  SourcesSpecSchema,
  StatGridSpecSchema,
  TableSpecSchema,
} from './content'
import { PickerSpecSchema, ScenarioSpecSchema, TicketSpecSchema } from './interactive'

/**
 * Every widget the chat can render, as the agent's tools emit them and the renderer reads
 * them. The same schemas run on both sides: a tool refuses what the renderer would refuse.
 */
export const WidgetSpecSchema = z.discriminatedUnion('kind', [
  ChartSpecSchema,
  CandlesSpecSchema,
  BookSpecSchema,
  MarketSpecSchema,
  StatsSpecSchema,
  PositionSpecSchema,
  RoundsSpecSchema,
  DistributionSpecSchema,
  FillsSpecSchema,
  MarkdownSpecSchema,
  CodeSpecSchema,
  SourcesSpecSchema,
  DiffSpecSchema,
  TableSpecSchema,
  StatGridSpecSchema,
  TicketSpecSchema,
  PickerSpecSchema,
  ScenarioSpecSchema,
])

export type WidgetSpec = z.infer<typeof WidgetSpecSchema>
export type WidgetKind = WidgetSpec['kind']

/** Kinds that read the exchange or signal stores; the rest carry their data inline. */
export const BOUND_KINDS: ReadonlySet<WidgetKind> = new Set<WidgetKind>([
  'chart',
  'candles',
  'book',
  'market',
  'stats',
  'position',
  'rounds',
  'distribution',
  'fills',
  'ticket',
  'scenario',
])

/** Tool output is untrusted evidence; the renderer re-validates before drawing. */
export function parseWidgetSpec(candidate: unknown): WidgetSpec | undefined {
  const parsed = z.safeParse(WidgetSpecSchema, candidate)
  return parsed.success ? parsed.data : undefined
}

export * from './bound'
export * from './content'
export * from './interactive'
export * from './refs'
