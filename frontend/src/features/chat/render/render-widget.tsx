'use client'

import { useChatData } from '../bindings/chat-data'
import { BOUND_KINDS, type WidgetSpec } from '../specs'
import { CodeBlock } from '../widgets/code-block'
import { Diff } from '../widgets/diff'
import { MarkdownBlock } from '../widgets/markdown-block'
import { Sources } from '../widgets/sources'
import { StatGrid } from '../widgets/stat-grid'
import {
  BookWidget,
  CandlesWidget,
  ChartWidget,
  DistributionWidget,
  FillsWidget,
  MarketWidget,
  PositionWidget,
  RoundsWidget,
  StatsWidget,
} from './bound-widgets'
import { AuthoredTable } from './authored-table'
import { WidgetAuthored, WidgetUnavailable } from './frames'
import { PickerWidget, ScenarioWidget, TicketWidget, type WidgetEvent } from './interactive-widgets'

const NOT_CONNECTED = 'This chat is not connected to an exchange, so there is nothing live to show.'

/**
 * A validated spec to the widget that draws it. Bound kinds need the chat's data context;
 * content kinds carry their own; interactive kinds send one event back through `onEvent`.
 */
export function RenderWidget({ spec, onEvent }: { spec: WidgetSpec; onEvent: WidgetEvent }) {
  const data = useChatData()
  if (data === null && BOUND_KINDS.has(spec.kind)) {
    return <WidgetUnavailable label={labelOf(spec)} reason={NOT_CONNECTED} />
  }
  switch (spec.kind) {
    case 'chart':
      return data ? <ChartWidget spec={spec} data={data} /> : null
    case 'candles':
      return data ? <CandlesWidget spec={spec} data={data} /> : null
    case 'book':
      return data ? <BookWidget spec={spec} data={data} /> : null
    case 'market':
      return data ? <MarketWidget spec={spec} data={data} /> : null
    case 'stats':
      return data ? <StatsWidget spec={spec} data={data} /> : null
    case 'position':
      return data ? <PositionWidget spec={spec} data={data} /> : null
    case 'rounds':
      return data ? <RoundsWidget spec={spec} data={data} /> : null
    case 'distribution':
      return data ? <DistributionWidget spec={spec} data={data} /> : null
    case 'fills':
      return data ? <FillsWidget spec={spec} data={data} /> : null
    case 'ticket':
      return data ? <TicketWidget spec={spec} data={data} onEvent={onEvent} /> : null
    case 'scenario':
      return data ? <ScenarioWidget spec={spec} data={data} /> : null
    case 'picker':
      return <PickerWidget spec={spec} onEvent={onEvent} />
    case 'markdown':
      return (
        <WidgetAuthored>
          <MarkdownBlock
            label={spec.title ?? 'Note'}
            title={spec.title ?? null}
            source={spec.text}
          />
        </WidgetAuthored>
      )
    case 'code':
      return (
        <WidgetAuthored>
          <CodeBlock
            label={spec.title ?? 'Code'}
            title={spec.title ?? null}
            {...(spec.language === undefined ? {} : { language: spec.language })}
            code={spec.code}
          />
        </WidgetAuthored>
      )
    case 'sources':
      return (
        <Sources
          label={spec.title ?? 'Sources'}
          title={spec.title ?? 'Sources'}
          meta={`${spec.sources.length} read`}
          sources={spec.sources.map((source, index) => ({
            id: `source-${index}`,
            title: source.title,
            url: source.url,
            ...(source.retrieved === undefined ? {} : { retrieved: source.retrieved }),
            ...(source.excerpt === undefined ? {} : { excerpt: source.excerpt }),
          }))}
        />
      )
    case 'diff':
      return (
        <WidgetAuthored>
          <Diff
            label={spec.title ?? 'What changed'}
            title={spec.title ?? 'What changed'}
            from={spec.from}
            to={spec.to}
            lines={spec.lines}
          />
        </WidgetAuthored>
      )
    case 'table':
      return <AuthoredTable spec={spec} />
    case 'stat-grid':
      return (
        <WidgetAuthored>
          <StatGrid
            label={spec.title ?? 'Figures'}
            title={spec.title ?? null}
            stats={spec.stats.map((stat, index) => ({
              id: `stat-${index}`,
              label: stat.label,
              value: stat.value,
              ...(stat.delta === undefined ? {} : { delta: stat.delta }),
              ...(stat.direction === undefined ? {} : { direction: stat.direction }),
            }))}
          />
        </WidgetAuthored>
      )
  }
}

function labelOf(spec: WidgetSpec): string {
  return 'title' in spec && spec.title !== undefined ? spec.title : spec.kind
}
