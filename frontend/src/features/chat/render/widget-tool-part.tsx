'use client'

import type { UIMessage } from 'ai'

import { parseWidgetSpec } from '../specs'
import { WidgetSkeleton, WidgetUnavailable } from './frames'
import type { WidgetEvent } from './interactive-widgets'
import { RenderWidget } from './render-widget'

/** The agent's widget tools are the ones whose names start with this: showChart, showBook… */
export const SHOW_TOOL_PREFIX = 'show'

type MessagePart = UIMessage['parts'][number]

/** Whether a message part is a call to one of the widget tools, streaming or finished. */
export function isWidgetToolPart(part: MessagePart): boolean {
  if (part.type.startsWith(`tool-${SHOW_TOOL_PREFIX}`)) return true
  return part.type === 'dynamic-tool' && part.toolName.startsWith(SHOW_TOOL_PREFIX)
}

/** What the widget tools return: the spec to draw, or why the request was refused. */
function specFromOutput(output: unknown) {
  if (typeof output !== 'object' || output === null) return { reason: 'The tool returned nothing.' }
  if ('ok' in output && output.ok === false) {
    const reason = 'reason' in output && typeof output.reason === 'string' ? output.reason : ''
    return { reason: reason || 'The widget was refused.' }
  }
  const candidate = 'spec' in output ? output.spec : output
  const spec = parseWidgetSpec(candidate)
  return spec === undefined ? { reason: 'The widget description could not be read.' } : { spec }
}

/**
 * One widget tool call as it moves through the stream: a skeleton while the model is still
 * writing the call, the widget once the tool answered, and the reason when it did not.
 * Tool output is untrusted evidence, so the spec is validated again here before drawing.
 */
export function WidgetToolPart({ part, onEvent }: { part: MessagePart; onEvent: WidgetEvent }) {
  if (!('state' in part)) return null
  const label = 'Widget'
  switch (part.state) {
    case 'input-streaming':
    case 'input-available':
      return <WidgetSkeleton label={label} />
    case 'output-error':
      return <WidgetUnavailable label={label} reason={part.errorText || 'The tool failed.'} />
    case 'output-available': {
      const resolved = specFromOutput(part.output)
      return 'spec' in resolved ? (
        <RenderWidget spec={resolved.spec} onEvent={onEvent} />
      ) : (
        <WidgetUnavailable label={label} reason={resolved.reason} />
      )
    }
    default:
      return null
  }
}
