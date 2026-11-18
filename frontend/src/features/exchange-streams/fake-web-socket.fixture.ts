/** A WebSocket stand-in for stream tests: records what was sent and lets a test emit frames. */
export class FakeWebSocket {
  static readonly OPEN = 1
  static instances: FakeWebSocket[] = []

  readonly listeners = new Map<string, Set<(event: Event) => void>>()
  readonly sent: string[] = []
  readyState = FakeWebSocket.OPEN
  closed = false
  readonly url: string

  constructor(url: string | URL) {
    this.url = String(url)
    FakeWebSocket.instances.push(this)
  }

  static reset() {
    FakeWebSocket.instances = []
  }

  addEventListener(type: string, listener: (event: Event) => void) {
    const listeners = this.listeners.get(type) ?? new Set()
    listeners.add(listener)
    this.listeners.set(type, listeners)
  }

  send(frame: string) {
    this.sent.push(frame)
  }

  close() {
    this.closed = true
  }

  open() {
    for (const listener of this.listeners.get('open') ?? []) listener(new Event('open'))
  }

  emit(data: unknown) {
    const event = new MessageEvent('message', { data: JSON.stringify(data) })
    for (const listener of this.listeners.get('message') ?? []) listener(event)
  }

  drop() {
    for (const listener of this.listeners.get('close') ?? []) listener(new Event('close'))
  }
}
