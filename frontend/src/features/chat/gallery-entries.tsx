import type { ReactNode } from 'react'

import {
  BarChartExample,
  CandleChartExample,
  DataTableExample,
  GaugeExample,
  HistogramExample,
  LadderExample,
  ScatterExample,
  StatGridExample,
  TimeSeriesExample,
} from './examples/data-examples'
import {
  CodeExample,
  DiffExample,
  MarkdownExample,
  SourcesExample,
} from './examples/content-examples'
import { PickerExample, SliderExample, TicketExample } from './examples/interactive-examples'
import { CardExample, HeatmapExample } from './examples/panel-examples'
import {
  EdgeGaugeExample,
  FillLogExample,
  MarketCardExample,
  OrderBookExample,
  PositionCardExample,
  PriceVsStrikeExample,
  RoundResultsExample,
  SignalChartExample,
  SignalCompareExample,
  StatusListExample,
} from './examples/preset-examples'

export interface GalleryEntry {
  id: string
  /** What the slot is for; a widget renders its own frame and label once it exists. */
  title: string
  render?: () => ReactNode
}

export interface GallerySection {
  id: string
  title: string
  description: string
  entries: readonly GalleryEntry[]
}

/**
 * Every widget kind the chat can render, in build order. An entry without `render`
 * shows as an empty frame until its component lands.
 */
export const GALLERY_SECTIONS: readonly GallerySection[] = [
  {
    id: 'data',
    title: 'Data types',
    description: 'Read live values. The agent names what to show; the frame reads the store.',
    entries: [
      { id: 'time-series', title: 'Time series', render: () => <TimeSeriesExample /> },
      { id: 'bar', title: 'Bar chart', render: () => <BarChartExample /> },
      { id: 'candles', title: 'Candlesticks', render: () => <CandleChartExample /> },
      { id: 'histogram', title: 'Histogram', render: () => <HistogramExample /> },
      { id: 'scatter', title: 'Scatter', render: () => <ScatterExample /> },
      { id: 'gauge', title: 'Gauge', render: () => <GaugeExample /> },
      { id: 'stat-grid', title: 'Stat grid', render: () => <StatGridExample /> },
      { id: 'table', title: 'Table', render: () => <DataTableExample /> },
      { id: 'ladder', title: 'Ladder', render: () => <LadderExample /> },
      { id: 'card', title: 'Card', render: () => <CardExample /> },
      { id: 'heatmap', title: 'Heatmap', render: () => <HeatmapExample /> },
    ],
  },
  {
    id: 'content',
    title: 'Content',
    description: 'Display tool output. Nothing here reads the tick store.',
    entries: [
      { id: 'markdown', title: 'Markdown', render: () => <MarkdownExample /> },
      { id: 'code', title: 'Code', render: () => <CodeExample /> },
      { id: 'sources', title: 'Sources', render: () => <SourcesExample /> },
      { id: 'diff', title: 'Diff', render: () => <DiffExample /> },
    ],
  },
  {
    id: 'interactive',
    title: 'Interactive',
    description: 'Send an event back into the conversation.',
    entries: [
      { id: 'ticket', title: 'Ticket', render: () => <TicketExample /> },
      { id: 'slider', title: 'Slider', render: () => <SliderExample /> },
      { id: 'picker', title: 'Picker', render: () => <PickerExample /> },
    ],
  },
  {
    id: 'presets',
    title: 'Presets',
    description: 'Named instances of the types above, the ones the agent emits by name.',
    entries: [
      { id: 'market-card', title: 'Market card', render: () => <MarketCardExample /> },
      { id: 'position-card', title: 'Position', render: () => <PositionCardExample /> },
      { id: 'signal-chart', title: 'Signal chart', render: () => <SignalChartExample /> },
      { id: 'signal-compare', title: 'Signal compare', render: () => <SignalCompareExample /> },
      { id: 'price-vs-strike', title: 'Price vs strike', render: () => <PriceVsStrikeExample /> },
      { id: 'order-book', title: 'Order book', render: () => <OrderBookExample /> },
      { id: 'status-list', title: 'Status list', render: () => <StatusListExample /> },
      { id: 'round-results', title: 'Round results', render: () => <RoundResultsExample /> },
      { id: 'fill-log', title: 'Fill log', render: () => <FillLogExample /> },
      { id: 'edge-gauge', title: 'Edge gauge', render: () => <EdgeGaugeExample /> },
    ],
  },
]

export const GALLERY_ENTRY_COUNT = GALLERY_SECTIONS.reduce(
  (count, section) => count + section.entries.length,
  0
)
