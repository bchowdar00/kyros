import { crossing, flip, stopTimer, take } from '../fragments'
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

const SHARE = '0.5'
const BACK = { reject: 'hunting', expiry: 'hunting' }

export const momentumFlip: Derivation = {
  id: 'momentum-flip',
  when: 'Buy whichever side gets rich, then flip to the other side when the held side falls.',
  sentence:
    'When either side goes above 70 cents buy it. If the side I hold then drops under 50 cents, flip my position to the other side. Hold to settlement.',
  brief: brief({
    summary:
      'Buys whichever side crosses 70 cents and flips to the other side if it falls under 50.',
    goal: 'Ride momentum on either side and flip when it reverses.',
    entry: 'either side crosses above 0.70',
    exit: 'flip to the other side when the held side falls below 0.50, else hold to settlement',
    outcome: 'either',
    trade_share: '50%',
  }),
  steps: [
    'Situations: hunting; holding up; holding down; done. "Flip" means two holding legs, one per side, so the flip trigger for each side is armed only while that side is held.',
    'Wakes: hunting hears both rich crossings and the stop timer. Holding up hears only up falling under 50; holding down hears only down falling under 50. Both hear the stop timer.',
    'Edge policy: every crossing is on_crossing + fresh_crossing. The flip trigger is a separate trigger with the opposite comparator, so the flip happens on the way down and never on the way up.',
    'Actions: a take moves to the matching holding leg. A flip is a reduce-only flatten of the held side followed by a take of the other side in the same block, then the other holding leg. The timer moves to done and the position rides to settlement.',
    'Memory: none. The flatten reads the signed position; the second order reads the dollars released.',
    'Sizes: takes spend 0.5 of the round budget. The flip buy spends abs(position) times avg_cost for a long, or times (1 - avg_cost) for a short, over the price paid.',
    'Fates: takes and flip buys route expiry and reject back to hunting. A flatten that fails leaves the position where it was, so it stays.',
  ],
  plan: {
    situations: ['hunting for a rich side', 'holding up', 'holding down', 'done for the round'],
    legs: [
      leg({
        name: 'hunting',
        wakes: [
          wake({ trigger: 'up_rich', phrase: 'up goes above 70 cents' }),
          wake({ trigger: 'down_rich', phrase: 'down goes above 70 cents' }),
          stopWake('10s'),
        ],
        actions: [
          act({
            on: 'up_rich',
            kind: 'take_up',
            tag: 'entry',
            share: SHARE,
            leads_to: 'holding_up',
          }),
          act({
            on: 'down_rich',
            kind: 'take_down',
            tag: 'entry',
            share: SHARE,
            leads_to: 'holding_down',
          }),
          stopAction('10s'),
        ],
      }),
      leg({
        name: 'holding_up',
        holds: 'up',
        wakes: [wake({ trigger: 'up_cheap', phrase: 'up drops under 50 cents' }), stopWake('10s')],
        actions: [
          act({ on: 'up_cheap', kind: 'flip', tag: 'flip', leads_to: 'holding_down' }),
          stopAction('10s'),
        ],
      }),
      leg({
        name: 'holding_down',
        holds: 'down',
        wakes: [
          wake({ trigger: 'down_cheap', phrase: 'down drops under 50 cents' }),
          stopWake('10s'),
        ],
        actions: [
          act({ on: 'down_cheap', kind: 'flip', tag: 'flip', leads_to: 'holding_up' }),
          stopAction('10s'),
        ],
      }),
      DONE_LEG,
    ],
    memory: [],
    fates: [
      { tag: 'entry', fill: null, expiry: 'hunting', reject: 'hunting' },
      { tag: 'flip', fill: null, expiry: 'hunting', reject: 'hunting' },
    ],
    stop_before_close: '10s',
  },
  document: {
    name: 'Momentum Flip',
    description:
      'On the 5-minute BTC up/down: when either side crosses above 70 cents, buy that side with half the round budget. While holding, if the held side falls below 50 cents, sell the whole position at market and buy the other side with the dollars released, then watch that side the same way. Ten seconds before the close, stop and hold to settlement.',
    ...bookOnly(),
    ...noExtras(),
    triggers: [
      crossing('up_rich', 'up', '>', '0.7'),
      crossing('down_rich', 'down', '>', '0.7'),
      crossing('up_cheap', 'up', '<', '0.5'),
      crossing('down_cheap', 'down', '<', '0.5'),
      stopTimer('last_10s', '10s'),
    ],
    legs: [
      { name: 'hunting', resident: true, arm: ['up_rich', 'down_rich', 'last_10s'] },
      { name: 'holding_up', resident: true, arm: ['up_cheap', 'last_10s'] },
      { name: 'holding_down', resident: true, arm: ['down_cheap', 'last_10s'] },
      DONE,
    ],
    blocks: [
      {
        leg: 'hunting',
        name: 'buy_up',
        guard: 'triggered(up_rich)',
        next_leg: 'holding_up',
        items: [take('up', SHARE, 'entry', BACK)],
      },
      {
        leg: 'hunting',
        name: 'buy_down',
        guard: 'triggered(down_rich)',
        next_leg: 'holding_down',
        items: [take('down', SHARE, 'entry', BACK)],
      },
      { leg: 'hunting', name: 'stop', guard: 'triggered(last_10s)', next_leg: 'done', items: [] },
      {
        leg: 'holding_up',
        name: 'flip_to_down',
        guard: 'triggered(up_cheap) and platform.position.size > 0',
        next_leg: 'holding_down',
        items: flip('up', 'flatten', 'flip', BACK),
      },
      {
        leg: 'holding_up',
        name: 'stop',
        guard: 'triggered(last_10s)',
        next_leg: 'done',
        items: [],
      },
      {
        leg: 'holding_down',
        name: 'flip_to_up',
        guard: 'triggered(down_cheap) and platform.position.size < 0',
        next_leg: 'holding_up',
        items: flip('down', 'flatten', 'flip', BACK),
      },
      {
        leg: 'holding_down',
        name: 'stop',
        guard: 'triggered(last_10s)',
        next_leg: 'done',
        items: [],
      },
    ],
  },
}
