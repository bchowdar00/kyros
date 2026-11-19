import { leaf, stopTimer, take, trigger } from '../fragments'
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

const SHARE = '0.1'

export const everyTime: Derivation = {
  id: 'every-time-with-cooldown',
  when: 'Act every time a condition holds, throttled so the round budget lasts.',
  sentence:
    'Whenever the up price is under 45 cents, buy some up, but not more than once every 5 seconds. Hold everything to settlement.',
  brief: brief({
    summary: 'Buys a little up every 5 seconds while the up price sits under 45 cents, and holds.',
    goal: 'Keep accumulating up while it is cheap.',
    entry: 'the up price is below 0.45, at most once every 5 seconds',
    exit: 'hold to settlement',
    recurrence: 'every_time',
    trade_share: '10%',
  }),
  steps: [
    'Situations: trading; done. Nothing changes mode on a fill, so one active leg.',
    'Wakes: trading hears the cheap condition and the stop timer.',
    'Edge policy: "whenever" is if_true + immediate, which fires on every book tick while true; a 5s cooldown paces it. No then, no gate.',
    'Actions: the wake takes up and stays; the timer moves to done.',
    'Memory: none.',
    'Sizes: each take spends 0.1 of the round budget, so about ten fires fit before the risk gate refuses the rest.',
    'Fates: none. A refused or unfilled take simply waits for the next wake.',
  ],
  plan: {
    situations: ['trading while cheap', 'done for the round'],
    legs: [
      leg({
        name: 'trading',
        wakes: [
          wake({
            trigger: 'cheap',
            phrase: 'up is under 45 cents',
            fire: 'if_true',
            rearm: 'immediate',
            cooldown: '5s',
          }),
          stopWake('10s'),
        ],
        actions: [
          act({ on: 'cheap', kind: 'take_up', tag: 'entry', share: SHARE }),
          stopAction('10s'),
        ],
      }),
      DONE_LEG,
    ],
    memory: [],
    fates: [],
    stop_before_close: '10s',
  },
  document: {
    name: 'Every Time Cooldown',
    description:
      'On the 5-minute BTC up/down: whenever the up price sits under 45 cents, buy up at once with a tenth of the round budget, at most once every 5 seconds. Hold everything to settlement. Ten seconds before the close, stop buying.',
    ...bookOnly(),
    ...noExtras(),
    triggers: [
      trigger({
        name: 'cheap',
        leaves: [
          leaf({
            signal: 'book',
            field: 'up',
            comparator: '<',
            value: '0.45',
            value_kind: 'literal',
          }),
        ],
        cooldown: '5s',
      }),
      stopTimer('last_10s', '10s'),
    ],
    legs: [{ name: 'trading', resident: true, arm: ['cheap', 'last_10s'] }, DONE],
    blocks: [
      {
        leg: 'trading',
        name: 'buy_up',
        guard: 'triggered(cheap)',
        next_leg: null,
        items: [
          {
            ...take('up', SHARE, 'entry', { reject: 'trading', expiry: 'trading' }),
            on_reject: null,
            on_expiry: null,
          },
        ],
      },
      { leg: 'trading', name: 'stop', guard: 'triggered(last_10s)', next_leg: 'done', items: [] },
    ],
  },
}
