import { EntityIdSchema } from '@kyros/domain'
import * as z from 'zod/mini'

export const ActiveMarketSchema = z.object({ eventId: EntityIdSchema })
