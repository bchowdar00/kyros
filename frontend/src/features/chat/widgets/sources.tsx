import type { ReactNode } from 'react'

import { WidgetFrame } from '../widget-frame'

export interface Source {
  id: string
  title: string
  url: string
  /** When the tool read it, already formatted for display. */
  retrieved?: string
  /** A one-line excerpt that supports the claim. */
  excerpt?: string
}

export interface SourcesProps {
  label: string
  title?: string | null
  meta?: ReactNode
  sources: readonly Source[]
}

/** Hosts a citation may link out to. Anything else renders as plain text. */
const LINKABLE_HOSTS = new Set([
  'coinbase.com',
  'pyth.network',
  'docs.pyth.network',
  'tiny.xyz',
  'bitcoin.org',
])

function host(url: string) {
  try {
    return new URL(url).hostname.replace(/^www\./, '')
  } catch {
    return null
  }
}

function linkable(hostname: string | null) {
  return hostname !== null && LINKABLE_HOSTS.has(hostname)
}

/** Numbered citations for a claim in the reply. The number is what the prose refers to. */
export function Sources({ label, title, meta, sources }: SourcesProps) {
  return (
    <WidgetFrame label={label} {...(title === undefined ? {} : { title })} meta={meta}>
      <ol className="flex flex-col gap-md">
        {sources.map((source, index) => {
          const hostname = host(source.url)
          return (
            <li key={source.id} className="flex gap-md">
              <span className="mt-px flex size-lg shrink-0 items-center justify-center rounded-xs bg-surface-raised font-mono text-caption text-foreground">
                {index + 1}
              </span>
              <div className="flex min-w-0 flex-col gap-copy">
                {linkable(hostname) ? (
                  <a
                    href={source.url}
                    rel="noopener noreferrer nofollow ugc"
                    target="_blank"
                    className="truncate text-body text-foreground underline-offset-2 hover:underline focus-visible:outline-2 focus-visible:outline-focus-ring"
                  >
                    {source.title}
                  </a>
                ) : (
                  <span className="truncate text-body text-foreground">{source.title}</span>
                )}
                <span className="truncate text-caption text-muted-foreground">
                  {hostname ?? 'unknown source'}
                  {source.retrieved ? ` · read ${source.retrieved}` : ''}
                </span>
                {source.excerpt ? (
                  <p className="border-l-2 border-border-subtle pl-sm text-label text-muted-foreground">
                    {source.excerpt}
                  </p>
                ) : null}
              </div>
            </li>
          )
        })}
      </ol>
    </WidgetFrame>
  )
}
