'use client'

import { Button } from '@kyros/ui/components/button'
import { useEffect, useState } from 'react'

import { WidgetFrame } from '../widget-frame'

export interface CodeBlockProps {
  label: string
  title?: string | null
  /** Shown beside the copy control, for example `json` or `strategy`. */
  language?: string
  code: string
}

/** How long the copy control confirms before returning to its resting label. */
const COPIED_MS = 1_600

/** Source or structured tool output, line-numbered, scrolling sideways inside the frame. */
export function CodeBlock({ label, title, language, code }: CodeBlockProps) {
  const [copied, setCopied] = useState(false)
  const lines = code.replace(/\n$/, '').split('\n')

  useEffect(() => {
    if (!copied) return undefined
    const timer = window.setTimeout(() => setCopied(false), COPIED_MS)
    return () => window.clearTimeout(timer)
  }, [copied])

  async function copy() {
    await navigator.clipboard.writeText(code)
    setCopied(true)
  }

  return (
    <WidgetFrame
      label={label}
      {...(title === undefined ? {} : { title })}
      meta={
        <span className="flex items-center gap-md">
          {language ? <span className="font-mono">{language}</span> : null}
          <Button
            size="small"
            variant="secondary"
            aria-label={copied ? 'Copied' : 'Copy code'}
            aria-live="polite"
            onClick={() => void copy()}
          >
            {copied ? 'Copied' : 'Copy'}
          </Button>
        </span>
      }
    >
      <pre className="-mx-lg overflow-x-auto px-lg font-mono text-label leading-relaxed text-foreground">
        <code className="grid grid-cols-[auto_1fr] gap-x-md">
          {lines.map((line, index) => (
            // Line numbers sit in the grid so they never enter a copy of the code itself.
            <span key={index} className="contents">
              <span aria-hidden="true" className="select-none text-right text-muted-foreground">
                {index + 1}
              </span>
              <span>{line || ' '}</span>
            </span>
          ))}
        </code>
      </pre>
    </WidgetFrame>
  )
}
