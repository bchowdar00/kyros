import { RequestContext } from '@mastra/core/request-context'
import { noopObserve } from '@mastra/core/tools'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createStrategyTools } from '@/mastra/capabilities/strategy/tools'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const tools = createStrategyTools('http://exchange.test')
function chatOn(marketSlug: string) {
  const requestContext = new RequestContext()
  requestContext.set('marketSlug', marketSlug)
  requestContext.set('resourceId', 'r')
  return { requestContext, observe: noopObserve }
}
const onMarket = chatOn('btc-2m')
const standalone = chatOn('standalone-chat')
const brief = {
  series: 'whatever',
  summary: 'Buys up once the price crosses 60 cents and sells at a 5 cent gain.',
  goal: 'Buy the dip',
  entry: 'RSI regime becomes oversold',
  signal_ids: ['btc.kraken.rsi.v1'],
  outcome: 'up' as const,
  recurrence: 'once_per_round' as const,
  interval: null,
  entry_style: 'market' as const,
  exit: 'hold to settlement',
  allocation: '100',
  round_share: '10%',
  trade_share: '25%',
  stop_after_loss: '100',
  stop_before_close: '10s',
  assumptions: [],
}

describe('strategy tools', () => {
  it('lists live signals for the chat’s market with the round length', async () => {
    server.use(
      http.get('http://exchange.test/api/signals', () =>
        HttpResponse.json({
          series: 'btc-2m',
          underlying: 'btc',
          signals: [
            {
              signal_id: 'btc-2m.current.price.v1',
              name: 'Market Price',
              description: 'Top of book. Up side.',
              location: 'internal',
              scope: { kind: 'series', series_id: 'btc-2m' },
              live: true,
              quality: { state: 'ready', reasons: [] },
              fields: [{ name: 'up', label: 'Up', type: 'decimal', headline: true }],
            },
          ],
        })
      ),
      http.get('http://exchange.test/api/series', () =>
        HttpResponse.json([
          {
            series: 'btc-2m',
            duration_secs: 120,
            current_market_id: 1,
            opens_at_ms: 1,
            closes_at_ms: 2,
          },
        ])
      )
    )
    expect(await tools.listMarketSignals.execute?.({}, onMarket)).toEqual({
      status: 'ok',
      series: 'btc-2m',
      roundSeconds: 120,
      signals: [
        {
          id: 'btc-2m.current.price.v1',
          name: 'Market Price',
          summary: 'Top of book.',
          fields: 'up:decimal',
        },
      ],
    })
  })

  it('is unavailable outside a market page', async () => {
    expect(await tools.listMarketSignals.execute?.({}, standalone)).toMatchObject({
      status: 'unavailable',
    })
  })

  it('proposes the brief pinned to the chat’s market without side effects', async () => {
    const proposal = await tools.proposeCompilation.execute?.(brief, onMarket)
    expect(proposal).toMatchObject({
      status: 'proposed',
      brief: { ...brief, series: 'btc-2m' },
    })
    expect(proposal).toHaveProperty('summary', expect.stringContaining('Market: btc-2m'))
    expect(proposal).toHaveProperty(
      'money',
      '$100 in, sees $10 per round, up to $2.50 per order, stops after losing $100'
    )
  })

  it('asks a catalogue question with its canned options and the recommendation', async () => {
    const asked = await tools.askQuestion.execute?.(
      { kind: 'side', question: '', options: [{ label: 'ignored', reason: '' }], recommended: 2 },
      onMarket
    )
    expect(asked).toEqual({
      status: 'awaiting_answer',
      kind: 'side',
      question: 'Which side should it buy?',
      options: [
        { label: 'Up', reason: '' },
        { label: 'Down', reason: '' },
        { label: 'Whichever side qualifies', reason: 'The rule picks the side when it fires' },
      ],
      recommended: 2,
    })
  })

  it('offers the NBA market’s own sides and fills them into the brief', async () => {
    const nba = [
      {
        series: 'nba-h2h',
        duration_secs: 300,
        underlying: 'nba',
        outcomes: ['home', 'away'],
        current_market_id: 1,
        opens_at_ms: 0,
        closes_at_ms: 1,
      },
    ]
    server.use(http.get('http://exchange.test/api/series', () => HttpResponse.json(nba)))
    const onNba = chatOn('nba-h2h')
    const asked = await tools.askQuestion.execute?.(
      { kind: 'side', question: '', options: [], recommended: null },
      onNba
    )
    expect(asked).toMatchObject({
      options: [{ label: 'Home' }, { label: 'Away' }, { label: 'Whichever side qualifies' }],
    })
    const proposal = await tools.proposeCompilation.execute?.({ ...brief, outcome: 'away' }, onNba)
    expect(proposal).toMatchObject({
      status: 'proposed',
      brief: { series: 'nba-h2h', outcome: 'away', sides: ['home', 'away'] },
    })
    expect(proposal).toHaveProperty(
      'summary',
      expect.stringContaining('Market: nba-h2h (sides: home, the quoted side, and away)')
    )
    // The quoted frame's names still read (`up` is the quoted side); a team name does not.
    expect(
      await tools.proposeCompilation.execute?.({ ...brief, outcome: 'up' }, onNba)
    ).toMatchObject({ status: 'proposed' })
    expect(
      await tools.proposeCompilation.execute?.({ ...brief, outcome: 'lakers' }, onNba)
    ).toEqual({
      status: 'side_problem',
      problem: '`lakers` is not a side of this market; it is home, away or either.',
    })
  })

  it('asks a custom question with short options and drops an out-of-range recommendation', async () => {
    const asked = await tools.askQuestion.execute?.(
      {
        kind: 'custom',
        question: 'Add to the position, or re-enter after selling?',
        options: [
          { label: 'Add to it', reason: 'Keeps buying on the way down' },
          { label: 'Re-enter after selling', reason: '' },
        ],
        recommended: 5,
      },
      onMarket
    )
    expect(asked).toMatchObject({ status: 'awaiting_answer', kind: 'custom', recommended: null })
    expect(asked).toHaveProperty('options', [
      { label: 'Add to it', reason: 'Keeps buying on the way down' },
      { label: 'Re-enter after selling', reason: '' },
    ])
  })

  it('refuses a custom question with one option or a long label', async () => {
    const one = await tools.askQuestion.execute?.(
      {
        kind: 'custom',
        question: 'q',
        options: [{ label: 'Only', reason: '' }],
        recommended: null,
      },
      onMarket
    )
    expect(one).toMatchObject({
      status: 'invalid',
      problem: expect.stringContaining('2 to 4 options'),
    })
    const long = await tools.askQuestion.execute?.(
      {
        kind: 'custom',
        question: 'q',
        options: [
          { label: 'this label has far too many words in it', reason: '' },
          { label: 'Short', reason: '' },
        ],
        recommended: null,
      },
      onMarket
    )
    expect(long).toMatchObject({
      status: 'invalid',
      problem: expect.stringContaining('longer than 6 words'),
    })
  })

  it('refuses to propose when the money does not reach one contract', async () => {
    const proposal = await tools.proposeCompilation.execute?.(
      { ...brief, allocation: '4' },
      onMarket
    )
    expect(proposal).toEqual({
      status: 'money_problem',
      problem:
        'Each round would see $0.40, less than one contract; raise the allocation or the round share.',
    })
  })
})
