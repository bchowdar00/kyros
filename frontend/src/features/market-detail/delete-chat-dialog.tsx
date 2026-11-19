'use client'

import { MiniXIcon } from '@kyros/ui/icons/mini-x-icon'
import { useEffect, useId, useRef } from 'react'
import type { SavedChat } from './saved-chat'

export interface DeleteChatDialogProps {
  chat: SavedChat
  deleting: boolean
  failed: boolean
  onConfirm: () => void
  onClose: () => void
}

/** One question before a chat and its messages go: keep it, or delete it for good. */
export function DeleteChatDialog({
  chat,
  deleting,
  failed,
  onConfirm,
  onClose,
}: DeleteChatDialogProps) {
  const titleId = useId()
  const cancelRef = useRef<HTMLButtonElement>(null)

  // Focus lands on the safe choice when the dialog opens; the opener returns it on close.
  useEffect(() => {
    cancelRef.current?.focus()
  }, [])

  return (
    <dialog
      open
      aria-labelledby={titleId}
      onKeyDown={event => {
        if (event.key === 'Escape' && !deleting) onClose()
      }}
      className="absolute inset-0 z-[2] m-0 flex size-full max-h-none max-w-none items-center justify-center bg-background/70 p-3xl text-foreground"
    >
      <button
        aria-label="Dismiss"
        className="absolute inset-0 cursor-default"
        disabled={deleting}
        onClick={onClose}
        type="button"
      />
      <article className="relative flex w-full max-w-content-copy flex-col gap-2xl rounded-md border-[0.5px] border-border-subtle bg-background p-3xl">
        <header className="flex items-start justify-between gap-lg">
          <div className="flex flex-col gap-xs">
            <p className="text-label text-muted-foreground">Delete chat</p>
            <h2 className="text-heading-sm font-regular text-foreground" id={titleId}>
              {chat.title}
            </h2>
          </div>
          <button
            aria-label="Close"
            className="min-h-touch min-w-touch rounded-xs p-sm text-muted-foreground outline-none focus-visible:outline-2 focus-visible:outline-focus-ring"
            disabled={deleting}
            onClick={onClose}
            type="button"
          >
            <MiniXIcon className="size-[18px]" />
          </button>
        </header>

        <p className="text-body text-foreground">
          This removes the conversation and everything Kyros said in it. There is no undo.
        </p>
        {failed ? (
          <p role="alert" className="text-label text-muted-foreground">
            The chat couldn’t be deleted. Try again.
          </p>
        ) : null}

        <footer className="flex items-center justify-end gap-lg">
          <button
            className="min-h-touch rounded-sm border-[0.5px] border-border-subtle px-xl text-body text-muted-foreground outline-none focus-visible:outline-2 focus-visible:outline-focus-ring"
            disabled={deleting}
            onClick={onClose}
            ref={cancelRef}
            type="button"
          >
            Keep chat
          </button>
          <button
            className="min-h-touch rounded-sm border-[0.5px] border-market-home px-xl text-body text-foreground outline-none focus-visible:outline-2 focus-visible:outline-focus-ring disabled:opacity-50"
            disabled={deleting}
            onClick={onConfirm}
            type="button"
          >
            {deleting ? 'Deleting…' : 'Delete chat'}
          </button>
        </footer>
      </article>
    </dialog>
  )
}
