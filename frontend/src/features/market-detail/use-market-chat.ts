'use client'

import { Chat, useChat } from '@ai-sdk/react'
import { DefaultChatTransport, type UIMessage } from 'ai'
import { useRef, useState } from 'react'
import { createApiClient } from '@tiny/api'
import { useIdentityStore } from '@/features/identity/identity-store'
import { chatTitle, SavedChatDetailSchema, type SavedChat } from './saved-chat'
import { useSavedChats } from './use-saved-chats'

export function useMarketChat(marketSlug: string | undefined) {
  const history = useSavedChats(marketSlug)
  const [transport] = useState(
    () =>
      new DefaultChatTransport({
        api: '/api/chat',
        prepareSendMessagesRequest: ({ id, messages, trigger }) => {
          // The account the user trades as reaches the agent's tools; read at send time
          // so a sign-in after the transport was built still counts.
          const accountAddress = useIdentityStore.getState().identity?.address
          return {
            body: {
              id,
              trigger,
              marketSlug: marketSlug ?? 'standalone-chat',
              ...(accountAddress === undefined ? {} : { accountAddress }),
              messages: messages
                .filter(message => message.role === 'user')
                .slice(-1)
                .map(message => ({
                  id: message.id,
                  role: message.role,
                  parts: message.parts.filter(part => part.type === 'text'),
                })),
            },
          }
        },
      })
  )
  const [sessions, setSessions] = useState(
    () => new Map<string, { chat: Chat<UIMessage>; summary: SavedChat }>()
  )
  const [chat, setChat] = useState(() => new Chat<UIMessage>({ transport }))
  const conversation = useChat({ chat, throttle: 100 })
  const [chatting, setChatting] = useState(false)
  const [title, setTitle] = useState('New chat')
  const [opening, setOpening] = useState<string | null>(null)
  const [openFailed, setOpenFailed] = useState(false)
  const openRequest = useRef<AbortController | null>(null)
  const [pendingDelete, setPendingDelete] = useState<SavedChat | null>(null)
  const [deleting, setDeleting] = useState(false)
  const [deleteFailed, setDeleteFailed] = useState(false)

  function submit(prompt: string) {
    if (chatting) {
      void conversation.sendMessage({ text: prompt })
      return
    }
    openRequest.current?.abort()
    setOpening(null)
    const next = new Chat<UIMessage>({
      transport,
      onFinish: () => history.refresh(),
      onError: () => history.refresh(),
    })
    setSessions(current =>
      new Map(current).set(next.id, {
        chat: next,
        summary: { id: next.id, title: chatTitle(prompt), updatedAt: new Date().toISOString() },
      })
    )
    setChat(next)
    setTitle(chatTitle(prompt))
    setChatting(true)
    setOpenFailed(false)
    void next.sendMessage({ text: prompt })
  }
  function back() {
    setChatting(false)
    history.refresh()
  }
  async function open(saved: SavedChat) {
    const local = sessions.get(saved.id)
    if (local) {
      setChat(local.chat)
      setTitle(local.summary.title)
      setChatting(true)
      return
    }
    openRequest.current?.abort()
    const controller = new AbortController()
    openRequest.current = controller
    setOpening(saved.id)
    setOpenFailed(false)
    const response = await createApiClient({
      baseUrl: window.location.origin,
      retry: { attempts: 1 },
    }).get(`/api/chats/${encodeURIComponent(saved.id)}`, {
      schema: SavedChatDetailSchema,
      signal: controller.signal,
    })
    if (controller.signal.aborted) return
    setOpening(null)
    if (!response.ok) {
      setOpenFailed(true)
      return
    }
    const reopened = new Chat<UIMessage>({
      id: saved.id,
      transport,
      messages: response.data.messages,
      onFinish: () => history.refresh(),
      onError: () => history.refresh(),
    })
    setSessions(current =>
      new Map(current).set(saved.id, { chat: reopened, summary: response.data.chat })
    )
    setChat(reopened)
    setTitle(response.data.chat.title)
    setChatting(true)
  }
  function askDelete(saved: SavedChat) {
    setDeleteFailed(false)
    setPendingDelete(saved)
  }
  function cancelDelete() {
    if (deleting) return
    setPendingDelete(null)
  }
  async function confirmDelete() {
    if (!pendingDelete || deleting) return
    setDeleting(true)
    setDeleteFailed(false)
    let removed = false
    try {
      // The browser sends the Origin header on a DELETE, which the route checks.
      const response = await fetch(`/api/chats/${encodeURIComponent(pendingDelete.id)}`, {
        method: 'DELETE',
      })
      // A chat the server no longer has is gone either way.
      removed = response.ok || response.status === 404
    } catch {
      removed = false
    }
    setDeleting(false)
    if (!removed) {
      setDeleteFailed(true)
      return
    }
    setSessions(current => {
      const next = new Map(current)
      next.delete(pendingDelete.id)
      return next
    })
    setPendingDelete(null)
    history.refresh()
  }
  return {
    ...conversation,
    chatting,
    title,
    submit,
    back,
    open,
    opening,
    openFailed,
    deletion: {
      pending: pendingDelete,
      deleting,
      failed: deleteFailed,
      ask: askDelete,
      cancel: cancelDelete,
      confirm: () => void confirmDelete(),
    },
    history: {
      ...history,
      chats: [
        ...Array.from(sessions.values())
          .toReversed()
          .map(session => ({
            ...session.summary,
            localStatus: history.chats.some(saved => saved.id === session.summary.id)
              ? undefined
              : session.chat.status === 'error'
                ? ('unsaved' as const)
                : ('saving' as const),
          })),
        ...history.chats.filter(saved => !sessions.has(saved.id)),
      ],
    },
  }
}
