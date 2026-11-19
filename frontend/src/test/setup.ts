import '@kyros/testing/setup'

// jsdom has no `matchMedia`. The theme switch reads the system colour scheme through it, so
// every test that renders the masthead needs one; a light system is the default and a test
// that cares stubs its own.
if (typeof window !== 'undefined' && typeof window.matchMedia !== 'function') {
  Object.defineProperty(window, 'matchMedia', {
    configurable: true,
    value: (query: string) => ({
      addEventListener: () => undefined,
      matches: false,
      media: query,
      removeEventListener: () => undefined,
    }),
    writable: true,
  })
}
