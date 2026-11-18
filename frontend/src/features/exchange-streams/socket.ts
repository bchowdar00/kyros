/** Ref-counting lives in the owning store; each channel has one reconnecting socket. */
export function openDemoSocket(
  url: string,
  handlers: {
    opened: (send: (frame: string) => boolean) => void
    message: (payload: unknown) => void
    disconnected: () => void
  }
) {
  let socket: WebSocket | undefined
  let timer: ReturnType<typeof setTimeout> | undefined
  let stopped = false
  let retries = 0
  const send = (frame: string) => {
    if (socket?.readyState !== WebSocket.OPEN) return false
    socket.send(frame)
    return true
  }
  const connect = () => {
    if (stopped) return
    socket = new WebSocket(url)
    socket.addEventListener('open', () => {
      if (!stopped) handlers.opened(send)
    })
    socket.addEventListener('message', event => {
      if (stopped) return
      try {
        const payload: unknown = JSON.parse(String(event.data))
        handlers.message(payload)
        retries = 0
      } catch {
        socket?.close(1000, 'Invalid exchange frame')
      }
    })
    socket.addEventListener('close', () => {
      if (stopped) return
      handlers.disconnected()
      const delay = Math.min(10000, 500 * 2 ** Math.min(retries++, 5))
      timer = setTimeout(connect, delay + Math.random() * 250)
    })
    socket.addEventListener('error', () => socket?.close())
  }
  connect()
  return {
    send,
    close() {
      stopped = true
      clearTimeout(timer)
      socket?.close()
    },
  }
}
