'use client'

import { useState } from 'react'
import { useMarketChat } from './use-market-chat'
import { openQuestion } from './open-question'
import { QuestionCard } from './question-card'
import { SavedChatCards } from './saved-chat-cards'
import { DeleteChatDialog } from './delete-chat-dialog'
import type { MarketTrading } from './exchange/buy'
import type { StrategyDeploy } from './exchange/deploy'
import { ChatMessages } from './chat-messages'
import type { ComponentPropsWithRef } from 'react'
import { cn } from '@kyros/ui/lib/cn'

import { LivePublicStrategiesList } from './live-public-strategies-list'
import { PublicStrategiesList, type PublicStrategiesListProps } from './public-strategies-list'
import type { PublicStrategyAction } from './public-strategy-card'
import { StrategyComposer, type StrategyComposerSubmitAction } from './strategy-composer'
import { ChatPanelInteraction } from './chat-panel-interaction'
import type { ExchangeApi } from './live-public-strategy-widget'
import { ChatPanelHeader } from './chat-panel-header'
import type { ChatPanelData, MarketOutcome } from './types'
import { useTradeTicket } from './use-trade-ticket'
import { ChatDataProvider } from '@/features/chat/bindings/chat-data'
import { useIdentityStore } from '@/features/identity/identity-store'

export interface ChatPanelProps extends Omit<ComponentPropsWithRef<'div'>, 'onSubmit'> {
  trading?: MarketTrading | undefined
  /** How the Run / Inverse widget deploys a template; absent without a live exchange. */
  deploy?: StrategyDeploy | undefined
  data: ChatPanelData
  marketSlug?: string
  marketKey?: string
  /** The exchange the chat's widgets bind to; absent on markets without one. */
  exchange?: { apiUrl: string; series: string } | undefined
  /** The exchange the strategy cards read their live template from; absent without one. */
  exchangeApi?: ExchangeApi | undefined
  outcomes?: readonly MarketOutcome[]
  scopeLabel?: string
  onRunStrategy?: PublicStrategyAction
  onInverseStrategy?: PublicStrategyAction
  onSubmit?: StrategyComposerSubmitAction
}

export function ChatPanel({
  data: panel,
  trading,
  deploy,
  marketKey = 'standalone-chat',
  marketSlug,
  outcomes = [],
  scopeLabel = 'This market',
  exchange,
  exchangeApi,
  className,
  onRunStrategy,
  onInverseStrategy,
  onSubmit,
  ...props
}: ChatPanelProps) {
  const chat = useMarketChat(marketSlug)
  const { messages, status, error, regenerate, stop, chatting } = chat
  const busy = status === 'submitted' || status === 'streaming'
  const { ticket } = useTradeTicket(marketKey)
  const address = useIdentityStore(state => state.identity?.address ?? null)
  const timeZone = new Intl.DateTimeFormat().resolvedOptions().timeZone
  const [dismissedQuestions, setDismissedQuestions] = useState<ReadonlySet<string>>(new Set())
  const question = chatting && !busy ? openQuestion(messages, dismissedQuestions) : undefined
  const listProps: Omit<PublicStrategiesListProps, 'strategies'> = {
    marketKey,
    ...(panel.defaultSortId === undefined ? {} : { defaultSortId: panel.defaultSortId }),
    ...(panel.sectionTitle === undefined ? {} : { sectionTitle: panel.sectionTitle }),
    ...(panel.sortLabel === undefined ? {} : { sortLabel: panel.sortLabel }),
    ...(panel.sortOptions === undefined ? {} : { sortOptions: panel.sortOptions }),
    ...(onRunStrategy === undefined ? {} : { onRunStrategy }),
    ...(onInverseStrategy === undefined ? {} : { onInverseStrategy }),
    ...(panel.availability === undefined ? {} : { availability: panel.availability }),
  }

  function submit(prompt: string) {
    if (status === 'submitted' || (chatting && busy)) return
    if (onSubmit) onSubmit(prompt)
    else chat.submit(prompt)
  }

  const transcript = (
    <ChatMessages
      messages={messages}
      pending={status === 'submitted'}
      failed={Boolean(error)}
      onRetry={() => void regenerate()}
      onAsk={submit}
      onDeploy={(build, brief) => ticket.openCompiled(build, brief, `deploy-${build.id}`)}
      onWidgetEvent={text => void chat.sendMessage({ text })}
    />
  )

  return (
    <div
      {...props}
      className={cn(
        // Border and padding are the chat-rail tokens: a widget on /chat measures against them.
        '@container relative flex h-full min-h-0 flex-col overflow-clip rounded-md border-2 border-foreground bg-background px-xl',
        className
      )}
    >
      <ChatPanelInteraction
        trading={trading}
        deploy={deploy}
        exchangeApi={exchangeApi}
        marketKey={marketKey}
        outcomes={outcomes}
        scopeLabel={scopeLabel}
        pinned={
          question ? (
            <QuestionCard
              key={question.id}
              question={question.output}
              onAnswer={submit}
              onDismiss={() => setDismissedQuestions(current => new Set(current).add(question.id))}
            />
          ) : null
        }
        composer={
          <StrategyComposer
            embedded
            focusInput={chatting}
            {...panel.composer}
            onSubmit={submit}
            key={chatting ? chat.id : 'new-chat'}
            busy={status === 'submitted' || (chatting && busy)}
            onStop={() => void stop()}
            showTip={!chatting}
            placeholder={
              chatting
                ? 'Message Kyros…'
                : (panel.composer?.placeholder ?? 'Write your own sentence…')
            }
            inputLabel="Message Kyros"
            submitLabel="Send message"
          />
        }
      >
        <ChatPanelHeader
          {...(chatting
            ? { title: chat.title, onBack: () => void chat.back() }
            : { busy: status === 'submitted', onBuild: submit })}
        />
        <section
          aria-label="Chat content"
          className="min-h-0 flex-1 overflow-y-auto pt-xl pb-xl overscroll-contain focus-visible:outline-2 focus-visible:outline-focus-ring [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
          // Scroll regions must be keyboard reachable (AGENTS.md §4).
          // oxlint-disable-next-line jsx-a11y/no-noninteractive-tabindex
          tabIndex={0}
        >
          {chatting ? (
            exchange === undefined ? (
              transcript
            ) : (
              <ChatDataProvider value={{ ...exchange, address, timeZone }}>
                {transcript}
              </ChatDataProvider>
            )
          ) : (
            <>
              <SavedChatCards
                {...chat.history}
                opening={chat.opening}
                onOpen={saved => void chat.open(saved)}
                onDelete={chat.deletion.ask}
                onRetry={chat.history.refresh}
                onLoadMore={chat.history.loadMore}
              />
              {chat.openFailed ? (
                <p role="alert" className="mb-lg text-label text-muted-foreground">
                  This chat couldn’t be opened. Select it to try again.
                </p>
              ) : null}
              {exchangeApi === undefined || panel.availability === 'unavailable' ? (
                <PublicStrategiesList {...listProps} strategies={panel.publicStrategies} />
              ) : (
                <LivePublicStrategiesList
                  {...listProps}
                  api={exchangeApi}
                  initial={panel.publicStrategies}
                  key={`strategies:${exchangeApi.series}`}
                />
              )}
            </>
          )}
        </section>
      </ChatPanelInteraction>
      {chat.deletion.pending ? (
        <DeleteChatDialog
          chat={chat.deletion.pending}
          deleting={chat.deletion.deleting}
          failed={chat.deletion.failed}
          onConfirm={chat.deletion.confirm}
          onClose={chat.deletion.cancel}
        />
      ) : null}
    </div>
  )
}
