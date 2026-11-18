import { bookTick, crossing, flattenLong, statement, stopTimer, take } from '../fragments'
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

export const reEntry: Derivation = {
  id: 're-entry-after-exit',
  when: 'Enter on a crossing, exit on profit or loss, then go back and wait for the next crossing.',
  sentence:
    'Every time the up price crosses above 60 cents buy up; sell at 5 cents profit or cut at 10 cents loss, then wait for the next crossing. Do this at most three times a round.',
  brief: brief({
    summary:
      'Buys up on each crossing of 60 cents, exits at +5 or -10 cents, and re-enters up to three times.',
    goal: 'Trade every fresh move above 60 cents, not just the first.',
    entry: 'the up price crosses above 0.60, up to 3 times a round',
    exit: 'sell at +0.05 over the average entry or cut at -0.10, then hunt again',
    recurrence: 'every_time',
  }),
  steps: [
    'Situations: hunting; holding; done. Re-entry is not a new leg: the exit leads back to hunting.',
    'Wakes: hunting hears the crossing and the stop timer; holding hears every book tick and the stop timer.',
    'Edge policy: the crossing is on_crossing + fresh_crossing, so after an exit the price must fall below 60 and cross again before the next entry. The tick is paced by the then out of holding.',
    'Actions: the take moves to holding; both exits flatten and move back to hunting; the timer moves to done.',
    'Memory: entries (u8), incremented by the entry block and read in its guard, caps the round at three.',
    'Sizes: each take spends 0.25 of the round budget; each flatten sells the whole position, reduce-only.',
    'Fates: the take routes expiry and reject back to hunting.',
  ],
  plan: {
    situations: ['hunting for a crossing', 'holding up', 'done for the round'],
    legs: [
      leg({
        name: 'hunting',
        wakes: [wake({ trigger: 'up_rich', phrase: 'up crosses above 60 cents' }), stopWake('10s')],
        actions: [
          act({
            on: 'up_rich',
            kind: 'take_up',
            tag: 'entry',
            share: SHARE,
            gate: 'entries',
            leads_to: 'holding',
          }),
          stopAction('10s'),
        ],
      }),
      leg({
        name: 'holding',
        holds: 'up',
        wakes: [tick('the bid moves'), stopWake('10s')],
        actions: [
          act({ on: 'book_tick', kind: 'flatten', tag: 'exit', leads_to: 'hunting' }),
          act({ on: 'book_tick', kind: 'flatten', tag: 'stop', leads_to: 'hunting' }),
          stopAction('10s'),
        ],
      }),
      DONE_LEG,
    ],
    memory: [{ slot: 'entries', type: 'u8', holds: 'entries taken this round' }],
    fates: [{ tag: 'entry', fill: null, expiry: 'hunting', reject: 'hunting' }],
    stop_before_close: '10s',
  },
  document: {
    name: 'Re-Entry',
    description:
      'On the 2-minute BTC up/down: when the up price crosses above 60 cents, buy up at once with a quarter of the round budget. While holding, sell the whole position at market once the bid is 5 cents above the average entry, or cut the loss when it falls 10 cents below; either way go back to hunting and wait for the next crossing above 60 cents. At most three entries per round. Ten seconds before the close, stop and hold whatever is open to settlement.',
    ...bookOnly(),
    state: [
      {
        name: 'entries',
        kind: 'scalar',
        value_type: 'u8',
        value_variants: null,
        capacity: null,
        key_types: null,
        max_keys: null,
      },
    ],
    params: [],
    named_expressions: [],
    tables: [],
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
        guard: 'triggered(up_rich) and state.entries < 3',
        next_leg: 'holding',
        items: [
          statement('set state.entries = state.entries + 1'),
          take('up', SHARE, 'entry', BACK),
        ],
      },
      { leg: 'hunting', name: 'stop', guard: 'triggered(last_10s)', next_leg: 'done', items: [] },
      {
        leg: 'holding',
        name: 'take_profit',
        guard:
          'triggered(book_tick) and platform.position.size > 0 and signal.book.bid >= platform.position.avg_cost + 0.05',
        next_leg: 'hunting',
        items: [flattenLong('exit')],
      },
      {
        leg: 'holding',
        name: 'cut_loss',
        guard:
          'triggered(book_tick) and platform.position.size > 0 and signal.book.bid <= platform.position.avg_cost - 0.1',
        next_leg: 'hunting',
        items: [flattenLong('stop')],
      },
      { leg: 'holding', name: 'stop', guard: 'triggered(last_10s)', next_leg: 'done', items: [] },
    ],
  },
}
