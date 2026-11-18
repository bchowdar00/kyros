/**
 * The Standard Schema interface (standardschema.dev). zod, valibot and arktype all
 * implement it, so this package validates responses without depending on any of them.
 */
export interface StandardSchema<Input = unknown, Output = Input> {
  readonly '~standard': {
    readonly version: 1
    readonly vendor: string
    readonly validate: (
      value: unknown
    ) => StandardSchemaResult<Output> | Promise<StandardSchemaResult<Output>>
    readonly types?: { readonly input: Input; readonly output: Output } | undefined
  }
}

export type StandardSchemaResult<Output> =
  | { readonly value: Output; readonly issues?: undefined }
  | { readonly issues: ReadonlyArray<StandardSchemaIssue> }

export interface StandardSchemaIssue {
  readonly message: string
  readonly path?: ReadonlyArray<PropertyKey | { readonly key: PropertyKey }> | undefined
}

export type InferOutput<Schema> =
  Schema extends StandardSchema<unknown, infer Output> ? Output : never

export function describeIssues(issues: ReadonlyArray<StandardSchemaIssue>): string {
  return issues
    .map(issue => {
      const path = (issue.path ?? [])
        .map(segment => (typeof segment === 'object' ? segment.key : segment))
        .map(String)
        .join('.')

      return path.length > 0 ? `${path}: ${issue.message}` : issue.message
    })
    .join('; ')
}
