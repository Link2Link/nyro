/**
 * Read a config field that may arrive as a plain scalar (plain
 * `schemastery` resolution — what this package depends on) or as a
 * volatile reference (the `@deepseek-ai/schemastery` fork used inside the
 * dsh host). Kept tolerant so either resolution feeds the routes.
 */

export function isVolatileRef(value: unknown): value is { get(): unknown } {
  return typeof value === 'object' && value !== null && typeof (value as { get?: unknown }).get === 'function'
}

export function derefVolatile<T>(value: unknown, fallback: T): T {
  if (isVolatileRef(value)) {
    const inner = value.get()
    return (inner === undefined || inner === null ? fallback : inner) as T
  }
  return (value === undefined || value === null ? fallback : value) as T
}
