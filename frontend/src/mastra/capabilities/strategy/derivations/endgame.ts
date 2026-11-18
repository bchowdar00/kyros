import { stopTimer, take } from '../fragments'
import { noExtras, act, bookOnly, brief, leg, wake, type Derivation } from './shared'

const SHARE = '0.25'

export const endgame: Derivation = {
  id: 'endgame-sweep',
  when: 'Act once at a fixed time before the round closes, based on where the book is then.',
  sentence:
    'Twenty seconds before the close, if either side is at 95 cents or more, buy that side and let it settle.',
  brief: brief({
    summary:
      'Twenty seconds before the close, buys whichever side is at 95 cents or more and lets it settle.',
    goal: 'Collect the last few cents on a decided round.',
    entry: '20s before the close, either side at or above 0.95',
    exit: 'hold to settlement',
    outcome: 'either',
    stop_before_close: '20s',
  }),
  steps: [
    'Situations: sweeping. One leg: the timer fires once a round and nothing else happens after it.',
    'Wakes: the 20-second window-clock timer, which is also the stop: nothing is placed after it.',
    'Edge policy: a window_clock timer is on_crossing + fresh_crossing and re-measures every round.',
    'Actions: two blocks on the same wake, one per side, each guarded on that side being at 95 or more. Both stay: the leg is resident and idles until settlement.',
    'Memory: none.',
    'Sizes: each take spends 0.25 of the round budget over the price paid; at 95 cents that is many contracts, capped by max_order_size.',
    'Fates: none needed. A miss is a miss until the next round.',
  ],
  plan: {
    situations: ['sweeping the endgame'],
    legs: [
      leg({
        name: 'sweeping',
        wakes: [
          wake({
            trigger: 'last_20s',
            phrase: '20 seconds before the close',
            source: 'window_clock',
          }),
        ],
        actions: [
          act({ on: 'last_20s', kind: 'take_up', tag: 'sweep', share: SHARE }),
          act({ on: 'last_20s', kind: 'take_down', tag: 'sweep', share: SHARE }),
        ],
      }),
    ],
    memory: [],
    fates: [],
    stop_before_close: '20s',
  },
  document: {
    name: 'Endgame Sweep',
    description:
      'On the 2-minute BTC up/down: 20 seconds before the round closes, if either side is trading at 95 cents or more, buy that side at once with a quarter of the round budget and let it settle. Every round.',
    ...bookOnly(),
    ...noExtras(),
    triggers: [stopTimer('last_20s', '20s')],
    legs: [{ name: 'sweeping', resident: true, arm: ['last_20s'] }],
    blocks: [
      {
        leg: 'sweeping',
        name: 'buy_up',
        guard: 'triggered(last_20s) and signal.book.up >= 0.95',
        next_leg: null,
        items: [
          {
            ...take('up', SHARE, 'sweep', { reject: 'sweeping', expiry: 'sweeping' }),
            on_reject: null,
            on_expiry: null,
          },
        ],
      },
      {
        leg: 'sweeping',
        name: 'buy_down',
        guard: 'triggered(last_20s) and signal.book.down >= 0.95',
        next_leg: null,
        items: [
          {
            ...take('down', SHARE, 'sweep', { reject: 'sweeping', expiry: 'sweeping' }),
            on_reject: null,
            on_expiry: null,
          },
        ],
      },
    ],
  },
}
