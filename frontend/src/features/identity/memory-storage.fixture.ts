// jsdom has no storage on an opaque test origin; a Map stands in for it.
export function memoryStorage(): Storage {
  const entries = new Map<string, string>()
  return {
    get length() {
      return entries.size
    },
    clear: () => entries.clear(),
    getItem: key => entries.get(key) ?? null,
    key: index => [...entries.keys()][index] ?? null,
    removeItem: key => void entries.delete(key),
    setItem: (key, value) => void entries.set(key, value),
  }
}
