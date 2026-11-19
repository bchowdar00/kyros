import { bookTick, crossing, flattenLong, stopTimer, take } from '../fragments'
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
  tick,
  wake,
  type Derivation,
} from './shared'

const SHARE = '0.25'
const BACK = { reject: 'hunting', expiry: 'hunting' }

export const threshold: Derivation = {
  id: 'threshold-entry-exit',
  when: 'Enter at market when a price crosses a level; exit on profit or loss, else hold.',
  sentence:
    'When the up price crosses above 60 cents buy up. Sell once the bid is 5 cents above what I paid, or cut it if it falls 10 cents below. Otherwise hold to settlement.',
  brief: brief({
    summary:
      'Buys up once the price crosses 60 cents and sells at a 5 cent gain or a 10 cent loss.',
    goal: 'Ride a move above 60 cents with a fixed profit target and stop.',
    entry: 'the up price crosses above 0.60',
    exit: 'sell at +0.05 over the average entry, cut at -0.10, else hold to settlement',
    signal_ids: [],
  }),
  steps: [
    'Situations: hunting for the crossing; holding the position; done. Three legs.',
    'Wakes: hunting hears the crossing and the stop timer; holding hears every book tick (the exit reads the bid against the average cost, which the SME cannot see) and the stop timer.',
    'Edge policy: "when it crosses" is on_crossing + fresh_crossing. The book tick is if_true + immediate; its blocks are paced by a then out of the leg.',
    'Actions: the crossing takes up and moves to holding; the exits flatten and move to done; the timer moves to done.',
    'Memory: none. The exit compares against platform.position.avg_cost.',
    'Sizes: the take spends 0.25 of the round budget over ask + 0.01; the flatten sells the whole position, reduce-only.',
    'Fates: the take routes expiry and reject back to hunting so a no-fill hunts again.',
  ],
  plan: {
    situations: ['hunting for the crossing', 'holding up', 'done for the round'],
    legs: [
      leg({
        name: 'hunting',
        wakes: [
          wake({ trigger: 'up_rich', phrase: 'the up price crosses above 60 cents' }),
          stopWake('10s'),
        ],
        actions: [
          act({ on: 'up_rich', kind: 'take_up', tag: 'entry', share: SHARE, leads_to: 'holding' }),
          stopAction('10s'),
        ],
      }),
      leg({
        name: 'holding',
        holds: 'up',
        wakes: [tick('the bid moves'), stopWake('10s')],
        actions: [
          act({ on: 'book_tick', kind: 'flatten', tag: 'exit', leads_to: 'done' }),
          act({ on: 'book_tick', kind: 'flatten', tag: 'stop', leads_to: 'done' }),
          stopAction('10s'),
        ],
      }),
      DONE_LEG,
    ],
    memory: [],
    fates: [{ tag: 'entry', fill: null, expiry: 'hunting', reject: 'hunting' }],
    stop_before_close: '10s',
  },
  document: {
    name: 'Threshold Entry',
    description:
      'On the 5-minute BTC up/down: when the up price crosses above 60 cents, buy up at once with a quarter of the round budget. While holding, sell the whole position at market once the bid is 5 cents above the average entry, or cut the loss when it falls 10 cents below. Otherwise hold to settlement. Once per round; nothing new in the last 10 seconds.',
    ...bookOnly(),
    ...noExtras(),
    triggers: [crossing('up_rich', 'up', '>', '0.6'), bookTick, stopTimer('last_10s', '10s')],
    legs: [
      { name: 'hunting', resident: true, arm: ['up_rich', 'last_10s'] },
      { name: 'holding', resident: true, arm: ['book_tick', 'last_10s'] },
      DONE,
    ],
    blocks: [
      {
        leg: 'hunting',
        name: 'buy_up',
        guard: 'triggered(up_rich)',
        next_leg: 'holding',
        items: [take('up', SHARE, 'entry', BACK)],
      },
      { leg: 'hunting', name: 'stop', guard: 'triggered(last_10s)', next_leg: 'done', items: [] },
      {
        leg: 'holding',
        name: 'take_profit',
        guard:
          'triggered(book_tick) and platform.position.size > 0 and signal.book.bid >= platform.position.avg_cost + 0.05',
        next_leg: 'done',
        items: [flattenLong('exit')],
      },
      {
        leg: 'holding',
        name: 'cut_loss',
        guard:
          'triggered(book_tick) and platform.position.size > 0 and signal.book.bid <= platform.position.avg_cost - 0.1',
        next_leg: 'done',
        items: [flattenLong('stop')],
      },
      { leg: 'holding', name: 'stop', guard: 'triggered(last_10s)', next_leg: 'done', items: [] },
    ],
  },
}
