/**
 * Routes under the full gate: zero WCAG 2.2 AA violations and byte budgets (AGENTS.md §9)
 * measured on the wire from the production server, on desktop. The market screen is a
 * desktop prototype that serializes a large fixture and joins this list when its data
 * loading is real; until then the smoke test still checks it renders and hydrates.
 */
export const gatedRoutes = [
  { name: 'home', path: '/', budget: { htmlKb: 40, jsKb: 170 } },
  // Waiver (AGENTS.md §9): /charts is a deliberately heavy three-chart showcase, and it is
  // where the shared chart system is proven before it ships anywhere else, so it carries the
  // price headline, the live tape and all three chart kinds at once. 187 KB pins today's
  // number so it can only tighten; 170 stays the floor for every route users actually visit.
  // The last 5 KB is @number-flow/react, which owns the headline's digit transitions.
  { name: 'charts', path: '/charts', budget: { htmlKb: 100, jsKb: 187 } },
] as const

export const smokeRoutes = [
  { name: 'home', path: '/' },
  { name: 'charts', path: '/charts' },
  { name: 'chat', path: '/chat' },
] as const

/**
 * Destinations the navigation links to before their routes exist (see `plannedRoute` in
 * src/lib/routes.ts). They answer a designed 404 (noindex) by intent. Next prefetches them
 * and logs a 404. Remove each entry when its route lands; the smoke test then fails on
 * that 404 again.
 */
export const plannedDestinations = new Set(['/leaderboard', '/profile', '/rewards', '/strategies'])
