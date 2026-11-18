import type { ReactNode } from 'react'

import { MarkdownProse } from '../markdown-prose'
import { WidgetFrame } from '../widget-frame'

export interface MarkdownBlockProps {
  label: string
  title?: string | null
  meta?: ReactNode
  source: string
}

/** Rendered Markdown from a tool or the model, limited to a safe subset, in a widget frame. */
export function MarkdownBlock({ label, title, meta, source }: MarkdownBlockProps) {
  return (
    <WidgetFrame label={label} {...(title === undefined ? {} : { title })} meta={meta}>
      <MarkdownProse source={source} />
    </WidgetFrame>
  )
}
