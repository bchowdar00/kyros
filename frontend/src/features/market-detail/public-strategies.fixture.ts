import { cents } from '@tiny/money'

import type { PublicStrategy } from './types'

const sampleRunners = [
  { src: '/assets/market/avatars/participant-01.png', alt: '' },
  { src: '/assets/market/avatars/participant-02.png', alt: '' },
  { src: '/assets/market/avatars/participant-03.png', alt: '' },
] as const

// Sports-flavoured cards for the static fixture routes and the component tests. Live
// routes read the exchange's template catalogue instead (`strategy-template.mapper.ts`).
// Source: https://www.figma.com/design/FsrHVbbzUmbmNO0MbXXfeu?node-id=1893-59594
export const PUBLIC_STRATEGY_FIXTURES = [
  {
    id: 'halftime-leaders-public',
    templateId: 'halftime-leaders-public',
    logic:
      'At halftime, buy the leading team’s outcome. Hold the position until the final buzzer and settle when the game ends.',
    inverseLogic:
      'At halftime, buy the trailing team’s outcome. Hold the position until the final buzzer and settle when the game ends.',
    runners: sampleRunners,
    title: 'Buy halftime leaders, hold to buzzer',
    creator: {
      handle: '@blknoiz06',
      username: 'blknoiz06',
      avatar: {
        alt: '@blknoiz06 avatar',
        src: '/assets/market/avatars/strategy-01.jpg',
      },
    },
    runningCount: 411,
    deployedCents: cents(208_000_000),
  },
  {
    id: 'fade-boston-runs',
    templateId: 'fade-boston-runs',
    logic:
      'When Boston scores eight unanswered points, buy the opposing team’s outcome. Sell after a six-cent recovery or hold to settlement.',
    inverseLogic:
      'When Boston scores eight unanswered points, buy Boston’s outcome. Sell after a six-cent gain or hold to settlement.',
    runners: sampleRunners,
    title: 'Fade Boston runs longer than eight points',
    creator: {
      handle: '@econoar',
      username: 'econoar',
      avatar: {
        alt: '@econoar avatar',
        src: '/assets/market/avatars/strategy-02.jpg',
      },
    },
    runningCount: 287,
    deployedCents: cents(29_400_000),
  },
  {
    id: 'momentum-flips-public',
    templateId: 'momentum-flips-public',
    logic:
      'When the lead changes, buy the new leader’s outcome. Exit that position when the lead changes again, then follow the new leader.',
    inverseLogic:
      'When the lead changes, buy the new trailing team’s outcome. Exit that position at the next lead change, then follow the new trailing team.',
    runners: sampleRunners,
    title: 'Buy the leader every time momentum flips',
    creator: {
      handle: '@maxvoltar',
      username: 'maxvoltar',
      avatar: {
        alt: '@maxvoltar avatar',
        src: '/assets/market/avatars/strategy-03.jpg',
      },
    },
    runningCount: 239,
    deployedCents: cents(184_000_000),
  },
  {
    id: 'sell-double-digit-lead',
    templateId: 'sell-double-digit-lead',
    logic:
      'When a team’s lead reaches ten points, sell any position held in that team. Wait for the lead to fall below ten before checking again.',
    inverseLogic:
      'When a team’s lead reaches ten points, buy that team’s outcome. Exit if the lead falls below ten points or hold to settlement.',
    runners: sampleRunners,
    title: 'Sell the leader when the lead hits double digits',
    creator: {
      handle: '@fabianrosholm',
      username: 'fabianrosholm',
      avatar: {
        alt: '@fabianrosholm avatar',
        src: '/assets/market/avatars/strategy-04.jpg',
      },
    },
    runningCount: 229,
    deployedCents: cents(46_900_000),
  },
  {
    id: 'fade-road-timeouts',
    templateId: 'fade-road-timeouts',
    logic:
      'When the visiting team calls back-to-back timeouts, buy the home team’s outcome. Sell after a six-cent gain or hold to settlement.',
    inverseLogic:
      'When the visiting team calls back-to-back timeouts, buy the visiting team’s outcome. Sell after a six-cent gain or hold to settlement.',
    runners: sampleRunners,
    title: 'Fade the road team after back-to-back timeouts',
    creator: {
      handle: '@lawmaster',
      username: 'lawmaster',
      avatar: {
        alt: '@lawmaster avatar',
        src: '/assets/market/avatars/strategy-05.jpg',
      },
    },
    runningCount: 226,
    deployedCents: cents(84_200_000),
  },
  {
    id: 'final-quarter-dips-public',
    templateId: 'final-quarter-dips-public',
    logic:
      'In the final quarter, buy an outcome when its price drops below 30¢. Sell after a six-cent recovery or hold to settlement.',
    inverseLogic:
      'In the final quarter, when an outcome drops below 30¢, buy the opposing outcome. Sell after a six-cent gain or hold to settlement.',
    runners: sampleRunners,
    title: 'Buy every dip below 30¢ in the final quarter',
    creator: {
      handle: '@mahdiofalltime',
      username: 'mahdiofalltime',
      avatar: {
        alt: '@mahdiofalltime avatar',
        src: '/assets/market/avatars/strategy-07.jpg',
      },
    },
    runningCount: 182,
    deployedCents: cents(14_800_000),
  },
  {
    id: 'final-quarter-dips-alt',
    templateId: 'final-quarter-dips-alt',
    logic:
      'During the final quarter, buy dips below 30¢ while the score is within six points. Sell at a six-cent gain or hold until the game ends.',
    inverseLogic:
      'During the final quarter, when an outcome dips below 30¢ and the score is within six points, buy the opposing outcome. Hold until the game ends.',
    runners: sampleRunners,
    title: 'Buy every dip below 30¢ in the final quarter',
    creator: {
      handle: '@heavyalien_',
      username: 'heavyalien_',
      avatar: {
        alt: '@heavyalien_ avatar',
        src: '/assets/market/avatars/strategy-08.jpg',
      },
    },
    runningCount: 174,
    deployedCents: cents(21_000_000),
  },
] as const satisfies readonly PublicStrategy[]
