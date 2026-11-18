import type { Route } from 'next'

/**
 * Typed routes make every `href` a compile-time check against the routes that exist. The
 * navigation already points at destinations that are still being built. This is the one
 * sanctioned cast for those, so the debt is greppable: delete each call when its route
 * lands, and the type system takes over.
 */
export function plannedRoute(pathname: `/${string}`): Route {
  return pathname as Route
}
