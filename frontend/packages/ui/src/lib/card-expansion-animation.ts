/** Shared motion for cards expanding from, or returning to, a composer. */
export class CardExpansionAnimation {
  readonly collapsed = { height: 0, opacity: 0 }
  readonly expanded = { height: 'auto', opacity: 1 }

  readonly transition

  constructor(reducedMotion: boolean | null) {
    this.transition = {
      type: 'tween' as const,
      // A deliberate expansion needs time to show its origin in the composer.
      duration: reducedMotion ? 0 : 0.35,
      ease: [0.1, 1, 0.2, 1] as const,
    }
  }
}
