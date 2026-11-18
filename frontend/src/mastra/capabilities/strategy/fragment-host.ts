import type { AuthoringDoItem, AuthoringStrategy } from './authoring-schema'
import { bookTick, stopTimer } from './fragments'

/**
 * The smallest document a fragment can live in: a hunting leg woken by every book
 * tick, a holding leg the fragments' fates may name, and done. Used by the tests that
 * prove every fragment compiles against the live venue, and by nothing else.
 */
export function fragmentHost(series: string, items: AuthoringDoItem[]): AuthoringStrategy {
  return {
    name: 'Fragment Host',
    description: 'A host document for one fragment: acts once on the first book tick, then stops.',
    instruments: [{ nick: 'market', id: `${series}.current` }],
    signals: [{ nick: 'book', id: `${series}.current.price.v1` }],
    state: [],
    params: [],
    named_expressions: [],
    tables: [],
    triggers: [bookTick, stopTimer('last_10s', '10s')],
    legs: [
      { name: 'hunting', resident: true, arm: ['book_tick', 'last_10s'] },
      { name: 'holding', resident: true, arm: ['last_10s'] },
      { name: 'done', resident: true, arm: [] },
    ],
    blocks: [
      { leg: 'hunting', name: 'act', guard: 'triggered(book_tick)', next_leg: 'done', items },
      { leg: 'hunting', name: 'stop', guard: 'triggered(last_10s)', next_leg: 'done', items: [] },
      { leg: 'holding', name: 'stop', guard: 'triggered(last_10s)', next_leg: 'done', items: [] },
    ],
  }
}
