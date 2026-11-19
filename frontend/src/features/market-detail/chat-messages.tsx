import type { UIMessage } from 'ai'
import { useEffect, useRef, useState } from 'react'
import * as z from 'zod/mini'

import { MarkdownProse } from '@/features/chat/markdown-prose'
import { isWidgetToolPart, WidgetToolPart } from '@/features/chat/render/widget-tool-part'
import type { StrategyBrief } from '@/mastra/capabilities/strategy/brief'
import { CompileProposalCard, CompileProposalSchema } from './compile-proposal-card'
import { QUESTION_PART, questionsIn } from './open-question'
import type { BuildStatus } from './strategy-build'

interface ChatMessagesProps {
  messages: UIMessage[]
  pending: boolean
  failed: boolean
  onRetry: () => void
  /** Sends a message on the user's behalf, e.g. a finished build's readback. */
  onAsk?: ((prompt: string) => void) | undefined
  /** Opens the deploy screen for a compiled build. */
  onDeploy?: ((build: BuildStatus, brief: StrategyBrief) => void) | undefined
  /** A widget fired; the sentence re-enters the conversation as the user's turn. */
  onWidgetEvent?: ((text: string) => void) | undefined
}

/** What Kyros is doing while a tool runs, by the tool's map key. */
const TOOL_ACTIVITY: Record<string, string> = {
  getLivePrice: 'Checking price feeds…',
  getPriceHistory: 'Checking price feeds…',
  listPriceAssets: 'Checking price feeds…',
  listMarketSignals: 'Looking up this market’s signals…',
  proposeCompilation: 'Preparing the plan…',
}

type MessagePart = UIMessage['parts'][number]
const PROPOSAL_PART = 'tool-proposeCompilation'

function isPendingTool(part: MessagePart) {
  return (
    (part.type.startsWith('tool-') || part.type === 'dynamic-tool') &&
    'state' in part &&
    (part.state === 'input-streaming' || part.state === 'input-available')
  )
}

/** The tool map key of a running tool part, or undefined for anything else. */
function toolName(part: MessagePart) {
  if (part.type === 'dynamic-tool') return part.toolName
  return part.type.startsWith('tool-') ? part.type.slice('tool-'.length) : undefined
}

/** The status line for the tools the last message is still running. */
function activityLabel(pendingTools: readonly MessagePart[]) {
  if (pendingTools.length === 0) return undefined
  if (pendingTools.some(isWidgetToolPart)) return 'Preparing a chart…'
  const name = pendingTools.map(toolName).find(candidate => candidate !== undefined)
  return (name === undefined ? undefined : TOOL_ACTIVITY[name]) ?? 'Kyros is working…'
}

export function ChatMessages({
  messages,
  pending,
  failed,
  onRetry,
  onAsk,
  onDeploy,
  onWidgetEvent,
}: ChatMessagesProps) {
  const activity = activityLabel(messages.at(-1)?.parts.filter(isPendingTool) ?? [])
  const [dismissed, setDismissed] = useState<ReadonlySet<string>>(new Set())
  const content = useRef<HTMLDivElement>(null)
  useEffect(() => {
    if (messages.length === 0 && !pending && !failed) return
    const scrollRegion = content.current?.closest('[aria-label="Chat content"]')
    if (scrollRegion) scrollRegion.scrollTop = scrollRegion.scrollHeight
  }, [messages, pending, failed])

  /** The user's reply to a question Kyros asked in `message`, once they have answered. */
  function answerTo(message: UIMessage) {
    const position = messages.indexOf(message)
    const reply = messages.slice(position + 1).find(next => next.role === 'user')
    return reply?.parts
      .filter(part => part.type === 'text')
      .map(part => part.text)
      .join(' ')
  }

  function renderPart(message: UIMessage, part: MessagePart, index: number) {
    const key = `${message.id}-${index}`
    if (part.type === QUESTION_PART) {
      // An open question is pinned above the composer; an answered one stays as a record.
      const answer = answerTo(message)
      const question = questionsIn(message).find(
        asked => 'toolCallId' in part && asked.id === part.toolCallId
      )
      if (!question || answer === undefined) return null
      return (
        <div key={key} className="pt-sm text-label text-muted-foreground">
          <p>{question.output.question}</p>
          <p className="text-foreground">
            <span aria-hidden="true">✓ </span>
            <span className="sr-only">Answered: </span>
            {answer}
          </p>
        </div>
      )
    }
    if (part.type === 'text') {
      // The user's words stay literal: they did not write Markdown, and a stray asterisk
      // must not italicise their message. The model's words are Markdown.
      if (message.role === 'user')
        return (
          <p key={key} className="whitespace-pre-wrap break-words text-body leading-relaxed">
            {part.text}
          </p>
        )
      return <MarkdownProse key={key} source={part.text} />
    }
    if (isWidgetToolPart(part))
      return <WidgetToolPart key={key} part={part} onEvent={text => onWidgetEvent?.(text)} />
    if (part.type !== PROPOSAL_PART || !('state' in part) || part.state !== 'output-available')
      return null
    const proposal = z.safeParse(CompileProposalSchema, part.output)
    if (!proposal.success || dismissed.has(part.toolCallId)) return null
    return (
      <div key={key} className="pt-sm">
        <CompileProposalCard
          brief={proposal.data.brief}
          onAsk={prompt => onAsk?.(prompt)}
          onKeepTalking={() => setDismissed(current => new Set(current).add(part.toolCallId))}
          onDeploy={onDeploy}
        />
      </div>
    )
  }

  return (
    <div ref={content} className="flex flex-col gap-xl">
      <div
        role="log"
        aria-label="Conversation with Kyros"
        aria-live="polite"
        className="flex flex-col gap-xl"
      >
        {messages.map(message => (
          <article
            key={message.id}
            aria-label={message.role === 'user' ? 'You' : 'Kyros'}
            className={
              message.role === 'user'
                ? 'max-w-full self-end rounded-md bg-action-muted px-lg py-sm text-foreground'
                : 'flex flex-col gap-lg text-foreground'
            }
          >
            <span className="sr-only">{message.role === 'user' ? 'You' : 'Kyros'}:</span>
            {message.parts.map((part, index) => renderPart(message, part, index))}
          </article>
        ))}
      </div>
      {activity !== undefined ? (
        <output className="text-label text-muted-foreground">{activity}</output>
      ) : pending ? (
        <output className="text-label text-muted-foreground">Kyros is thinking…</output>
      ) : null}
      {failed ? (
        <div role="alert" className="text-body text-foreground">
          <p>Kyros couldn’t finish this response. Your message is saved here.</p>
          <button
            type="button"
            onClick={onRetry}
            className="mt-sm underline focus-visible:outline-2 focus-visible:outline-focus-ring"
          >
            Try again
          </button>
        </div>
      ) : null}
    </div>
  )
}
