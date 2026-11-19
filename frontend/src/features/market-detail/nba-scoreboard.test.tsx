import type { RustSignalFrame } from '@kyros/domain'
import { expectNoA11yViolations } from '@kyros/testing/a11y'
import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'

import { NbaScoreboardStrip } from './nba-scoreboard'

const scoreboard = { home: 'DEN', away: 'BOS', tipoffMs: Date.now() + 3_600_000 }

function frame(values: RustSignalFrame['values']): RustSignalFrame {
  return {
    signal_id: 'nba.live.pbp.v1',
    ts_ms: 1,
    quality: { state: 'ready', reasons: [] },
    values,
  }
}

describe('nba scoreboard', () => {
  it('shows the score, clock and last play when the frame matches', async () => {
    const { container } = render(
      <NbaScoreboardStrip
        scoreboard={scoreboard}
        values={
          frame({
            home_team: 'DEN',
            away_team: 'BOS',
            phase: 'in_play',
            quarter: 3,
            clock_remaining: '562',
            home_score: 55,
            away_score: 65,
            action: 'two_pointer',
            result: 'made',
            team: 'BOS',
            player: '',
          }).values
        }
      />
    )
    expect(screen.getByRole('region', { name: 'Game score' })).toBeVisible()
    expect(screen.getByText('BOS 65')).toBeVisible()
    expect(screen.getByText('DEN 55')).toBeVisible()
    expect(screen.getByText('Q3 9:22')).toBeVisible()
    expect(screen.getByText('BOS two-pointer made')).toBeVisible()
    await expectNoA11yViolations(container)
  })

  it('shows dashes for a frame from another game', () => {
    render(
      <NbaScoreboardStrip
        scoreboard={scoreboard}
        values={
          frame({
            home_team: 'LAL',
            away_team: 'MIA',
            phase: 'in_play',
            quarter: 2,
            clock_remaining: '300',
            home_score: 40,
            away_score: 38,
          }).values
        }
      />
    )
    expect(screen.getByText('BOS —')).toBeVisible()
    expect(screen.getByText('DEN —')).toBeVisible()
  })

  it('shows the tipoff line before the game, and no invented score', () => {
    render(
      <NbaScoreboardStrip
        scoreboard={scoreboard}
        values={frame({ home_team: 'DEN', away_team: 'BOS', phase: 'pregame' }).values}
      />
    )
    expect(screen.getByText(/^Tipoff /)).toBeVisible()
    expect(screen.getByText('BOS —')).toBeVisible()
    expect(screen.getByText('DEN —')).toBeVisible()
  })

  it('shows the scheduled tipoff when no frame has arrived, whatever the time is now', () => {
    render(<NbaScoreboardStrip scoreboard={{ ...scoreboard, tipoffMs: Date.now() - 3_600_000 }} />)
    expect(screen.getByText(/^Tipoff /)).toBeVisible()
  })

  it('says so when the score feed is disconnected', () => {
    render(
      <NbaScoreboardStrip
        feed="disconnected"
        scoreboard={scoreboard}
        values={
          frame({
            home_team: 'DEN',
            away_team: 'BOS',
            phase: 'in_play',
            quarter: 3,
            clock_remaining: '562',
            home_score: 55,
            away_score: 65,
          }).values
        }
      />
    )
    expect(screen.getByText('BOS 65')).toBeVisible()
    expect(screen.getByText('Score feed disconnected')).toBeVisible()
  })

  it('shows Final once the game ends', () => {
    render(
      <NbaScoreboardStrip
        scoreboard={scoreboard}
        values={
          frame({
            home_team: 'DEN',
            away_team: 'BOS',
            phase: 'final',
            home_score: 108,
            away_score: 121,
          }).values
        }
      />
    )
    expect(screen.getByText('Final')).toBeVisible()
    expect(screen.getByText('BOS 121')).toBeVisible()
    expect(screen.getByText('DEN 108')).toBeVisible()
  })

  it('renders the dashes state with no accessibility violations and no frame', async () => {
    const { container } = render(<NbaScoreboardStrip scoreboard={scoreboard} />)
    expect(screen.getByText('BOS —')).toBeVisible()
    await expectNoA11yViolations(container)
  })
})
