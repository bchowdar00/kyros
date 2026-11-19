import { cents, probabilityBps } from '@kyros/money'
import * as z from 'zod/mini'

/** Series, Event, Market and Outcome ids are independent six-digit integers. */
export const EntityIdSchema = z.int().check(z.minimum(100_001), z.maximum(999_999))

/** Identity strings are stored lowercase by the backend. */
export const SlugSchema = z.string().check(z.regex(/^[a-z0-9]+(?:-[a-z0-9]+)*$/))

/** A category is a path: `crypto/bitcoin/up-or-down/5m`. */
export const CategoryPathSchema = z.string().check(z.regex(/^[a-z0-9-]+(?:\/[a-z0-9-]+)*$/))

export const IsoDateTimeSchema = z.iso.datetime()

/** Integer minor units on the wire become the branded `Cents` type in the app. */
export const CentsSchema = z.pipe(z.int(), z.transform(cents))

/** A probability in basis points on the wire becomes the branded `Bps` type in the app. */
export const ProbabilityBpsSchema = z.pipe(
  z.int().check(z.minimum(0), z.maximum(10_000)),
  z.transform(probabilityBps)
)

export type EntityId = z.infer<typeof EntityIdSchema>
