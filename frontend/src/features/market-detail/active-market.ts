import { EntityIdSchema } from '@tiny/domain'
import * as z from 'zod/mini'

export const ActiveMarketSchema = z.object({ eventId: EntityIdSchema })
