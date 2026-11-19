import { ForwardIcon } from '@kyros/ui/icons/forward-icon'
import { MiniXIcon } from '@kyros/ui/icons/mini-x-icon'
import type { SavedChat } from './saved-chat'

interface SavedChatCardsProps {
  chats: SavedChat[]
  loading: boolean
  failed: boolean
  hasMore: boolean
  opening: string | null
  onOpen: (chat: SavedChat) => void
  onDelete: (chat: SavedChat) => void
  onRetry: () => void
  onLoadMore: () => void
}
export function SavedChatCards({
  chats,
  loading,
  failed,
  hasMore,
  opening,
  onOpen,
  onDelete,
  onRetry,
  onLoadMore,
}: SavedChatCardsProps) {
  if (!chats.length && !loading && !failed) return null
  return (
    <section aria-label="Your chats" className="mb-xl flex flex-col gap-md">
      <h3 className="text-label font-medium text-foreground">Your chats</h3>
      {chats.map(chat => (
        <div
          key={chat.id}
          className="group flex min-h-touch w-full items-center gap-sm rounded-md border-[0.5px] border-border-subtle/50 bg-surface-subtle p-lg hover:bg-surface-raised"
        >
          <button
            type="button"
            onClick={() => onOpen(chat)}
            disabled={opening !== null}
            aria-label={`Open chat: ${chat.title}`}
            className="flex min-h-touch min-w-0 flex-1 items-center justify-between gap-md rounded-sm text-left focus-visible:outline-2 focus-visible:outline-focus-ring"
          >
            <span className="min-w-0">
              <span className="block truncate text-body text-foreground">{chat.title}</span>
              <span className="block text-label text-muted-foreground">
                {opening === chat.id
                  ? 'Opening…'
                  : chat.localStatus === 'unsaved'
                    ? 'Not saved — reopen to retry'
                    : chat.localStatus === 'saving'
                      ? 'Saving conversation…'
                      : 'Continue conversation'}
              </span>
            </span>
            <ForwardIcon className="size-icon-md shrink-0 text-muted-foreground" />
          </button>
          {/* Only a chat the server knows can be deleted; a saving one has nothing to remove yet. */}
          {chat.localStatus === undefined ? (
            <button
              type="button"
              onClick={() => onDelete(chat)}
              disabled={opening !== null}
              aria-label={`Delete chat: ${chat.title}`}
              className="flex min-h-touch min-w-touch shrink-0 items-center justify-center rounded-sm text-muted-foreground opacity-60 hover:text-foreground hover:opacity-100 focus-visible:opacity-100 focus-visible:outline-2 focus-visible:outline-focus-ring group-hover:opacity-100"
            >
              <MiniXIcon className="size-[18px]" />
            </button>
          ) : null}
        </div>
      ))}
      {loading ? (
        <output className="text-label text-muted-foreground">Loading chats…</output>
      ) : null}
      {failed ? (
        <div role="alert" className="text-label text-muted-foreground">
          Saved chats couldn’t be loaded.{' '}
          <button type="button" className="underline" onClick={onRetry}>
            Retry loading chats
          </button>
        </div>
      ) : null}
      {hasMore && !loading ? (
        <button
          type="button"
          className="min-h-touch text-label text-muted-foreground underline"
          onClick={onLoadMore}
        >
          Load more chats
        </button>
      ) : null}
    </section>
  )
}
