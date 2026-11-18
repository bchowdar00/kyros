import { bookTick, crossing, statement, stopTimer, take } from '../fragments'
import {
  DONE,
  DONE_LEG,
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

export const scaleIn: Derivation = {
  id: 'scale-in',
  when: 'Enter, then add the same amount each time the price falls a step, up to a count.',
  sentence:
    'Buy up when it crosses 55 cents, and every time it drops another 5 cents below my last buy add the same amount again, up to three adds. Hold to settlement.',
  brief: brief({
    summary:
      'Buys up at 55 cents and adds a quarter each 5 cent drop, up to three times, then holds.',
    goal: 'Average down into an up position.',
    entry:
      'the up price crosses above 0.55; add on each 0.05 drop below the last buy, at most 3 adds',
    exit: 'hold to settlement',
  }),
  steps: [
    'Situations: hunting for the first crossing; holding and adding; done. One holding leg with a counter, not four legs.',
    'Wakes: hunting hears the crossing and the stop timer; holding hears every book tick (the add compares the ask to a remembered price) and the stop timer.',
    'Edge policy: the crossing is on_crossing + fresh_crossing. The tick is if_true + immediate and is paced by the counter and the remembered price the add block itself rewrites.',
    'Actions: the first take moves to holding. The add stays in holding: it increments the counter, moves the remembered price down, and takes again. The timer moves to done.',
    'Memory: adds (u8) and last_entry (decimal), both written in the block that reads them so a wake cannot fire twice on the same drop.',
    'Sizes: every take spends 0.25 of the round budget over ask + 0.01, so four entries fit the round.',
    'Fates: the first take routes expiry and reject back to hunting. The adds stay: a missed add is simply retried on the next qualifying tick.',
  ],
  plan: {
    situations: [
      'hunting for the first crossing',
      'holding and adding on the way down',
      'done for the round',
    ],
    legs: [
      leg({
        name: 'hunting',
        wakes: [wake({ trigger: 'up_rich', phrase: 'up crosses 55 cents' }), stopWake('10s')],
        actions: [
          act({ on: 'up_rich', kind: 'take_up', tag: 'entry', share: SHARE, leads_to: 'holding' }),
          stopAction('10s'),
        ],
      }),
      leg({
        name: 'holding',
        holds: 'up',
        wakes: [tick('the ask drops 5 cents below the last buy'), stopWake('10s')],
        actions: [
          act({ on: 'book_tick', kind: 'take_up', tag: 'add', share: SHARE, gate: 'adds' }),
          stopAction('10s'),
        ],
      }),
      DONE_LEG,
    ],
    memory: [
      { slot: 'adds', type: 'u8', holds: 'how many adds so far this round' },
      { slot: 'last_entry', type: 'decimal', holds: 'the price of the last buy' },
    ],
    fates: [{ tag: 'entry', fill: null, expiry: 'hunting', reject: 'hunting' }],
    stop_before_close: '10s',
  },
  document: {
    name: 'Scale In',
    description:
      'On the 2-minute BTC up/down: when the up price crosses above 55 cents, buy up at once with a quarter of the round budget and remember the price paid. While holding, every time the ask sits at least 5 cents under the last entry, add another quarter and remember the new price, at most three adds per round. Hold to settlement. Ten seconds before the close, stop adding.',
    ...bookOnly(),
    state: [
      {
        name: 'adds',
        kind: 'scalar',
        value_type: 'u8',
        value_variants: null,
        capacity: null,
        key_types: null,
        max_keys: null,
      },
      {
        name: 'last_entry',
        kind: 'scalar',
        value_type: 'decimal',
        value_variants: null,
        capacity: null,
        key_types: null,
        max_keys: null,
      },
    ],
    params: [],
    named_expressions: [],
    tables: [],
    triggers: [crossing('up_rich', 'up', '>', '0.55'), bookTick, stopTimer('last_10s', '10s')],
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
        items: [
          statement('set state.last_entry = signal.book.ask + 0.01'),
          take('up', SHARE, 'entry', BACK),
        ],
      },
      { leg: 'hunting', name: 'stop', guard: 'triggered(last_10s)', next_leg: 'done', items: [] },
      {
        leg: 'holding',
        name: 'add',
        guard:
          'triggered(book_tick) and state.adds < 3 and signal.book.ask <= state.last_entry - 0.05',
        next_leg: null,
        items: [
          statement('set state.adds = state.adds + 1'),
          statement('set state.last_entry = signal.book.ask + 0.01'),
          { ...take('up', SHARE, 'add', BACK), on_reject: null, on_expiry: null },
        ],
      },
      { leg: 'holding', name: 'stop', guard: 'triggered(last_10s)', next_leg: 'done', items: [] },
    ],
  },
}
