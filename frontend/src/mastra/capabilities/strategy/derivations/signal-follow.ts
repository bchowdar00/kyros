import { leaf, stopTimer, take, trigger } from '../fragments'
import {
  DONE,
  DONE_LEG,
  MARKET,
  BOOK,
  RSI,
  act,
  brief,
  leg,
  stopAction,
  stopWake,
  wake,
  type Derivation,
} from './shared'

const SHARE = '0.25'
const BACK = { reject: 'watching', expiry: 'watching' }

export const signalFollow: Derivation = {
  id: 'signal-follow',
  when: 'Trade on an external indicator (RSI, momentum, order flow) with a user-tunable parameter.',
  sentence: 'When the RSI goes oversold, buy up and hold to settlement.',
  brief: brief({
    summary: 'Buys up when the Kraken RSI turns oversold and holds to settlement.',
    goal: 'Buy the dip the RSI flags.',
    entry: 'the RSI regime becomes oversold',
    exit: 'hold to settlement',
    signal_ids: [RSI],
  }),
  steps: [
    'Situations: watching; done. There is no exit rule, so no holding leg is needed: the position rides to settlement.',
    'Wakes: watching hears the regime change and the stop timer.',
    'Edge policy: "when it goes oversold" is a field leaf on the regime compared to a param, on_crossing + fresh_crossing, so it fires once when the regime changes, not on every RSI publish while oversold.',
    'Actions: the take moves to done. The timer moves to done.',
    'Memory: none. The regime is a deploy-time param so the user can change it on a running strategy.',
    'Sizes: the take spends 0.25 of the round budget over ask + 0.01; a guard keeps it from paying over 90 cents.',
    'Fates: the take routes expiry and reject back to watching.',
  ],
  plan: {
    situations: ['watching the RSI', 'done for the round'],
    legs: [
      leg({
        name: 'watching',
        wakes: [wake({ trigger: 'regime_hit', phrase: 'the RSI goes oversold' }), stopWake('10s')],
        actions: [
          act({ on: 'regime_hit', kind: 'take_up', tag: 'entry', share: SHARE, leads_to: 'done' }),
          stopAction('10s'),
        ],
      }),
      DONE_LEG,
    ],
    memory: [],
    fates: [{ tag: 'entry', fill: null, expiry: 'watching', reject: 'watching' }],
    stop_before_close: '10s',
  },
  document: {
    name: 'RSI Oversold Buy',
    description:
      'On the 5-minute BTC up/down: when the Kraken RSI regime becomes the chosen regime (oversold by default), buy up at once with a quarter of the round budget and hold to settlement. Nothing happens in the last 10 seconds. Once per round.',
    instruments: [{ nick: 'market', id: MARKET }],
    signals: [
      { nick: 'rsi', id: RSI },
      { nick: 'book', id: BOOK },
    ],
    state: [],
    params: [
      {
        name: 'regime',
        type: 'enum',
        variants: ['oversold', 'neutral', 'overbought'],
        default: 'oversold',
      },
    ],
    named_expressions: [],
    tables: [],
    triggers: [
      trigger({
        name: 'regime_hit',
        leaves: [
          leaf({
            signal: 'rsi',
            field: 'regime',
            comparator: '==',
            value: 'regime',
            value_kind: 'param',
          }),
        ],
        fire: 'on_crossing',
        rearm: 'fresh_crossing',
      }),
      stopTimer('last_10s', '10s'),
    ],
    legs: [{ name: 'watching', resident: true, arm: ['regime_hit', 'last_10s'] }, DONE],
    blocks: [
      {
        leg: 'watching',
        name: 'buy_up',
        guard: 'triggered(regime_hit) and signal.book.up < 0.9',
        next_leg: 'done',
        items: [take('up', SHARE, 'entry', BACK)],
      },
      { leg: 'watching', name: 'stop', guard: 'triggered(last_10s)', next_leg: 'done', items: [] },
    ],
  },
}
