/**
 * Browser-side API client for the /api/nyro-usage route family. The only
 * data access path the panel uses — plain same-origin fetch.
 */

import type {
  NyroInstanceStatus,
  NyroInstanceTestResult,
  NyroInstanceUsage,
  NyroStatusResponse,
  NyroTestRouteResponse,
  NyroUsageItem,
  NyroUsageRouteResponse,
} from '../nyro.ts'

export type {
  NyroInstanceStatus,
  NyroInstanceTestResult,
  NyroInstanceUsage,
  NyroStatusResponse,
  NyroTestRouteResponse,
  NyroUsageItem,
  NyroUsageRouteResponse,
}

/** Error carrying the route's JSON error message. */
export class NyroPanelApiError extends Error {
  /** Route error kind when the host provided one (e.g. `unauthorized`). */
  readonly kind: string | undefined

  constructor(message: string, kind?: string) {
    super(message)
    this.name = 'NyroPanelApiError'
    this.kind = kind
  }
}

/** Parse a JSON response or throw a NyroPanelApiError. */
async function readJson<T>(response: Response): Promise<T> {
  let body: unknown
  try {
    body = await response.json()
  } catch {
    throw new NyroPanelApiError(`HTTP ${response.status}: invalid JSON response`)
  }
  if (!response.ok) {
    const record = typeof body === 'object' && body !== null ? body as { error?: unknown; kind?: unknown } : {}
    const message = typeof record.error === 'string' && record.error !== '' ? record.error : `HTTP ${response.status}`
    const kind = typeof record.kind === 'string' ? record.kind : undefined
    throw new NyroPanelApiError(message, kind)
  }
  return body as T
}

/** Build a route URL with optional `instance` / `refresh` query params. */
function routeUrl(path: string, params: { instance?: string; refresh?: boolean }): string {
  const search = new URLSearchParams()
  if (params.instance !== undefined) search.set('instance', params.instance)
  if (params.refresh === true) search.set('refresh', '1')
  const query = search.toString()
  return query === '' ? path : `${path}?${query}`
}

/** The host proxy routes (same origin). */
export class NyroPanelApi {
  /** Sanitized config + per-gateway cache state. */
  async status(): Promise<NyroStatusResponse> {
    return readJson<NyroStatusResponse>(await fetch('/api/nyro-usage/status'))
  }

  /**
   * One gateway's provider usage.
   * @param instance - gateway id from the status route.
   * @param refresh - bypass the host-side cache.
   * @returns that gateway's slice (its own failure is reported inside it).
   */
  async usage(instance: string, refresh = false): Promise<NyroInstanceUsage> {
    const body = await readJson<NyroUsageRouteResponse>(await fetch(routeUrl('/api/nyro-usage/usage', { instance, refresh })))
    const slice = body.instances.find(entry => entry.id === instance)
    if (slice === undefined) throw new NyroPanelApiError('unknown nyro instance', 'not-found')
    return slice
  }

  /**
   * Connectivity + auth test against the saved configuration.
   * @param instance - one gateway id; omitted tests every gateway.
   * @returns the overall verdict plus one result per tested gateway.
   */
  async test(instance?: string): Promise<NyroTestRouteResponse> {
    return readJson<NyroTestRouteResponse>(await fetch(routeUrl('/api/nyro-usage/test', { instance }), { method: 'POST' }))
  }
}
