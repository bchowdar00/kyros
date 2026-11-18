import { flattenLong, flattenShort, leaf, stopTimer, take, trigger } from '../fragments'
import type { Sides } from '../sides'
import {
  DONE,
  DONE_LEG,
  noExtras,
  act,
  brief,
  leg,
  stopAction,
  stopWake,
  wake,
  type Derivation,
} from './shared'

/**
 * The NBA shape: a public model prices the game and the strategy buys whichever side
 * the book has cheaper than the model, then sells out once the book comes back. It is
 * the worked derivation in home/away labels, so a writer on an NBA series sees its own
 * market's words, and it is the same strategy the exchange ships as the model-vs-crowd
 * template.
 */
const SERIES = 'nba-h2h'
const NBA: Sides = { quoted: 'home', other: 'away' }
const SHARE = '0.25'
const BACK = { reject: 'hunting', expiry: 'hunting' }
const EDGE = '0.04'

/** "Whenever the gap is at least the edge": every publish while true, paced by a cooldown. */
function gap(name: string, expr: string, value: string) {
  return trigger({
    name,
    leaves: [leaf({ expr, comparator: '>=', value, value_kind: 'literal' })],
    fire: 'if_true',
    rearm: 'immediate',
    cooldown: '1s',
  })
}

function gapWake(name: string, phrase: string) {
  return wake({ trigger: name, phrase, fire: 'if_true', rearm: 'immediate', cooldown: '1s' })
}

export const modelVsCrowd: Derivation = {
  id: 'model-vs-crowd',
  when: 'Buy whichever side a model says is cheap, sell out when the book agrees with the model, and hunt again (an NBA game, home against away).',
  sentence:
    'Whenever either team is at least four cents under the model win probability at the ask, buy it. Once the book comes back to the model, sell out and go back to hunting. Nothing new in the last fifteen seconds.',
  brief: brief({
    series: SERIES,
    sides: ['home', 'away'],
    summary:
      'Buys whichever team the book prices at least four cents under the model, sells out once the book reaches the model, and hunts again.',
    goal: 'Trade the gap between the public win-probability model and the crowd.',
    entry:
      'the model home win probability (nba.sim.winprob.v1 home) is at least 0.04 above the home ask, or at least 0.04 below the home bid (away is cheap)',
    exit: 'sell out once the book comes back to the model, then hunt again',
    signal_ids: ['nba.sim.winprob.v1'],
    outcome: 'either',
    recurrence: 'every_time',
    stop_before_close: '15s',
  }),
  steps: [
    'Situations: hunting for a cheap side; holding home; holding away; done. Four legs: either side can be held, and the exit leads back to hunting.',
    'Wakes: hunting hears home cheap and away cheap; holding home hears only home at fair; holding away hears only away at fair. Every leg hears the stop timer.',
    'Edge policy: "whenever the gap is at least the edge" is if_true + immediate, paced by a 1s cooldown and by the then out of the leg. The gap is an expr leaf over the model and the book, so the SME evaluates it on every publish of either.',
    'Actions: home cheap takes home (the quoted side) and moves to holding home; away cheap takes away and moves to holding away; at fair flattens the held side and moves back to hunting; the timer moves to done.',
    'Memory: none. The guards read the position to refuse a second entry and to sell only what is held.',
    'Sizes: each take spends 0.25 of the round budget over the price paid (ask + 0.01 for home, 1 - bid + 0.01 for away); each flatten sells the whole position, reduce-only, on the side the leg holds.',
    'Fates: both takes route expiry and reject back to hunting so a no-fill hunts again.',
  ],
  plan: {
    situations: ['hunting for a cheap side', 'holding home', 'holding away', 'done for the round'],
    legs: [
      leg({
        name: 'hunting',
        wakes: [
          gapWake('home_cheap', 'home is at least four cents under the model at the ask'),
          gapWake('away_cheap', 'away is at least four cents under the model at the ask'),
          stopWake('15s'),
        ],
        actions: [
          act({
            on: 'home_cheap',
            kind: 'take_up',
            tag: 'entry',
            share: SHARE,
            leads_to: 'holding_home',
          }),
          act({
            on: 'away_cheap',
            kind: 'take_down',
            tag: 'entry',
            share: SHARE,
            leads_to: 'holding_away',
          }),
          stopAction('15s'),
        ],
      }),
      leg({
        name: 'holding_home',
        holds: 'up',
        wakes: [gapWake('home_at_fair', 'the home bid reaches the model'), stopWake('15s')],
        actions: [
          act({ on: 'home_at_fair', kind: 'flatten', tag: 'exit', leads_to: 'hunting' }),
          stopAction('15s'),
        ],
      }),
      leg({
        name: 'holding_away',
        holds: 'down',
        wakes: [gapWake('away_at_fair', 'the away bid reaches the model'), stopWake('15s')],
        actions: [
          act({ on: 'away_at_fair', kind: 'flatten', tag: 'exit', leads_to: 'hunting' }),
          stopAction('15s'),
        ],
      }),
      DONE_LEG,
    ],
    memory: [],
    fates: [{ tag: 'entry', fill: null, expiry: 'hunting', reject: 'hunting' }],
    stop_before_close: '15s',
  },
  document: {
    name: 'Model vs Crowd',
    description:
      'On the NBA head-to-head: the public win-probability model prices the game from the score, the clock and the pregame line. Whenever either side sits at least four cents under the model at the ask, buy it at once with a quarter of the round budget. While holding, sell the whole position at market once the bid on that side reaches the model, then hunt again, so a game can be traded more than once. Fifteen seconds before the close, stop and hold whatever is open to settlement.',
    instruments: [{ nick: 'market', id: `${SERIES}.current` }],
    signals: [
      { nick: 'book', id: `${SERIES}.current.price.v1` },
      { nick: 'model', id: 'nba.sim.winprob.v1' },
    ],
    ...noExtras(),
    triggers: [
      gap('home_cheap', 'signal.model.home - signal.book.ask', EDGE),
      gap('away_cheap', 'signal.book.bid - signal.model.home', EDGE),
      gap('home_at_fair', 'signal.book.bid - signal.model.home', '0'),
      gap('away_at_fair', 'signal.model.home - signal.book.ask', '0'),
      stopTimer('last_15s', '15s'),
    ],
    legs: [
      { name: 'hunting', resident: true, arm: ['home_cheap', 'away_cheap', 'last_15s'] },
      { name: 'holding_home', resident: true, arm: ['home_at_fair', 'last_15s'] },
      { name: 'holding_away', resident: true, arm: ['away_at_fair', 'last_15s'] },
      DONE,
    ],
    blocks: [
      {
        leg: 'hunting',
        name: 'buy_home',
        guard:
          'triggered(home_cheap) and abs(platform.position.size) < 1 and signal.book.ask < 0.95',
        next_leg: 'holding_home',
        items: [take('up', SHARE, 'entry', BACK, NBA)],
      },
      {
        leg: 'hunting',
        name: 'buy_away',
        guard:
          'triggered(away_cheap) and abs(platform.position.size) < 1 and 1 - signal.book.bid < 0.95',
        next_leg: 'holding_away',
        items: [take('down', SHARE, 'entry', BACK, NBA)],
      },
      { leg: 'hunting', name: 'stop', guard: 'triggered(last_15s)', next_leg: 'done', items: [] },
      {
        leg: 'holding_home',
        name: 'sell_home',
        guard: 'triggered(home_at_fair) and platform.position.size >= 1',
        next_leg: 'hunting',
        items: [flattenLong('exit', NBA)],
      },
      {
        leg: 'holding_home',
        name: 'stop',
        guard: 'triggered(last_15s)',
        next_leg: 'done',
        items: [],
      },
      {
        leg: 'holding_away',
        name: 'sell_away',
        guard: 'triggered(away_at_fair) and platform.position.size <= -1',
        next_leg: 'hunting',
        items: [flattenShort('exit', NBA)],
      },
      {
        leg: 'holding_away',
        name: 'stop',
        guard: 'triggered(last_15s)',
        next_leg: 'done',
        items: [],
      },
    ],
  },
}
