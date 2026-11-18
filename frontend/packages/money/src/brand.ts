declare const brand: unique symbol

/**
 * Nominal typing for plain numbers. A `Cents` cannot be passed where `Bps` is expected
 * even though both are numbers at runtime, and neither can be built from a bare literal
 * without going through its constructor.
 */
export type Brand<T, Name extends string> = T & { readonly [brand]: Name }
