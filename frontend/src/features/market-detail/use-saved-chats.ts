'use client'

import { QueryClient, useInfiniteQuery } from '@tanstack/react-query'
import { useState } from 'react'
import { createApiClient } from '@tiny/api'
import { SavedChatsSchema } from './saved-chat'

export function useSavedChats(marketSlug: string | undefined) {
  const [client] = useState(() => new QueryClient())
  const queryKey = ['saved-chats', marketSlug]
  const query = useInfiniteQuery(
    {
      queryKey,
      enabled: Boolean(marketSlug),
      initialPageParam: 0,
      queryFn: async ({ signal, pageParam }) => {
        const response = await createApiClient({
          baseUrl: window.location.origin,
          retry: { attempts: 1 },
        }).get('/api/chats', {
          schema: SavedChatsSchema,
          searchParams: { marketSlug, page: pageParam },
          signal,
        })
        if (!response.ok) throw response.error
        return response.data
      },
      getNextPageParam: (last, pages) => (last.hasMore ? pages.length : undefined),
      retry: false,
    },
    client
  )
  return {
    chats: query.data?.pages.flatMap(page => page.chats) ?? [],
    loading: query.isFetching,
    failed: query.isError,
    hasMore: query.hasNextPage,
    refresh: () => {
      void client.resetQueries({ queryKey })
    },
    loadMore: () => {
      void query.fetchNextPage()
    },
  }
}
