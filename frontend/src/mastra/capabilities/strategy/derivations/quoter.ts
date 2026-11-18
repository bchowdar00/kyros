import { cancel, interval, leaf, requote, restAsk, restBid, stopTimer, trigger } from '../fragments'
import {
  DONE,
  DONE_LEG,
  noExtras,
  act,
  bookOnly,
  brief,
  leg,
  stopAction,
  stopWake,
  wake,
  type Derivation,
} from './shared'

const SHARE = '0.25'

export const quoter: Derivation = {
  id: 'two-sided-quoter',
  when: 'Rest a bid and an ask together, follow the book, pull both before the close.',
  sentence:
    'When the spread is at least 4 cents wide, quote both sides: a bid a cent above the best bid and an ask a cent below the best ask. Keep them following the book every 2 seconds and pull them 10 seconds before the close.',
  brief: brief({
    summary:
      'Quotes a bid and an ask inside a wide spread, re-quotes every 2 seconds, pulls before the close.',
    goal: 'Earn the spread as a maker.',
    entry:
      'the up spread (ask - bid) is at least 0.04 wide; rest a bid at bid + 0.01 and an ask at ask - 0.01',
    exit: 'a filled side is held to settlement; both quotes are pulled 10s before the close',
    entry_style: 'limit',
    recurrence: 'on_interval',
    interval: '2s',
  }),
  steps: [
    'Situations: hunting for a wide spread; quoting; done. Both quotes live in one leg under two tags.',
    'Wakes: hunting hears the wide-spread condition and the stop timer; quoting hears a 2-second interval and the stop timer.',
    'Edge policy: "when the spread is wide" as an entry gate is if_true + immediate with a 2s cooldown, paced by the then into quoting. The interval is on_crossing + immediate and restarts when quoting is entered.',
    'Actions: the entry rests both tags and moves to quoting. The interval re-quotes both in place; a filled tag simply has nothing to replace. The timer cancels both tags and moves to done.',
    'Memory: none.',
    'Sizes: each quote spends 0.25 of the round budget; the ask divides by the complement of its price because a sell posts (1 - price) of collateral.',
    'Fates: resting orders stay on fill; the cancel-replace is self-limiting and needs none.',
  ],
  plan: {
    situations: ['hunting for a wide spread', 'quoting both sides', 'done for the round'],
    legs: [
      leg({
        name: 'hunting',
        wakes: [
          wake({
            trigger: 'wide',
            phrase: 'the spread is at least 4 cents',
            fire: 'if_true',
            rearm: 'immediate',
            cooldown: '2s',
          }),
          stopWake('10s'),
        ],
        actions: [
          act({ on: 'wide', kind: 'rest_bid', tag: 'bid', share: SHARE, leads_to: 'quoting' }),
          act({ on: 'wide', kind: 'rest_ask', tag: 'ask', share: SHARE, leads_to: 'quoting' }),
          stopAction('10s'),
        ],
      }),
      leg({
        name: 'quoting',
        holds: 'either',
        wakes: [
          wake({
            trigger: 'every_2s',
            phrase: 'every 2 seconds',
            source: 'interval',
            rearm: 'immediate',
          }),
          stopWake('10s'),
        ],
        actions: [
          act({ on: 'every_2s', kind: 'requote', tag: 'bid', share: SHARE }),
          act({ on: 'every_2s', kind: 'requote', tag: 'ask', share: SHARE }),
          act({ on: 'last_10s', kind: 'cancel', tag: 'bid', leads_to: 'done' }),
          act({ on: 'last_10s', kind: 'cancel', tag: 'ask', leads_to: 'done' }),
        ],
      }),
      DONE_LEG,
    ],
    memory: [],
    fates: [],
    stop_before_close: '10s',
  },
  document: {
    name: 'Two-Sided Quoter',
    description:
      'On the 2-minute BTC up/down: when the up spread is at least 4 cents wide, rest a bid one cent above the best bid and an ask one cent below the best ask, each spending a quarter of the round budget. Re-quote both every 2 seconds to follow the book; a filled side is simply not re-quoted. Ten seconds before the close, pull both and stop.',
    ...bookOnly(),
    ...noExtras(),
    triggers: [
      trigger({
        name: 'wide',
        leaves: [
          leaf({
            expr: 'signal.book.ask - signal.book.bid',
            comparator: '>=',
            value: '0.04',
            value_kind: 'literal',
          }),
        ],
        cooldown: '2s',
      }),
      interval('every_2s', '2s'),
      stopTimer('last_10s', '10s'),
    ],
    legs: [
      { name: 'hunting', resident: true, arm: ['wide', 'last_10s'] },
      { name: 'quoting', resident: true, arm: ['every_2s', 'last_10s'] },
      DONE,
    ],
    blocks: [
      {
        leg: 'hunting',
        name: 'quote',
        guard: 'triggered(wide)',
        next_leg: 'quoting',
        items: [restBid(SHARE, 'bid', null), restAsk(SHARE, 'ask', null)],
      },
      { leg: 'hunting', name: 'stop', guard: 'triggered(last_10s)', next_leg: 'done', items: [] },
      {
        leg: 'quoting',
        name: 'requote',
        guard: 'triggered(every_2s)',
        next_leg: null,
        items: [requote('bid', SHARE, 'bid'), requote('ask', SHARE, 'ask')],
      },
      {
        leg: 'quoting',
        name: 'pull',
        guard: 'triggered(last_10s)',
        next_leg: 'done',
        items: [cancel('bid'), cancel('ask')],
      },
    ],
  },
}
