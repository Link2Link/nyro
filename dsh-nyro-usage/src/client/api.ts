/**
 * Browser-side API client for the /api/nyro-usage route family. The only
 * data access path the panel uses — plain same-origin fetch.
 */

import type { NyroStatusResponse, NyroTestResult, NyroUsageItem } from '../nyro.ts'

/** Bulk usage response (items + cache metadata). */
export interface UsageResponse {
  data: NyroUsageItem[]
  cachedAt: string
}

export type { NyroStatusResponse, NyroTestResult, NyroUsageItem }

/** Error carrying the route's JSON error message. */
export class NyroPanelApiError extends Error {
  constructor(
    message: string,
    /** Route error kind when the host provided one (e.g. `unauthorized`). */
    readonly kind?: string,
  ) {
    super(message)
    this.name = 'NyroPanelApiError'
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

/** The host proxy routes (same origin). */
export class NyroPanelApi {
  /** Sanitized config + cache state. */
  async status(): Promise<NyroStatusResponse> {
    return readJson<NyroStatusResponse>(await fetch('/api/nyro-usage/status'))
  }

  /** Every provider's usage; `refresh` bypasses the host-side cache. */
  async usage(refresh = false): Promise<UsageResponse> {
    const search = refresh ? '?refresh=1' : ''
    return readJson<UsageResponse>(await fetch(`/api/nyro-usage/usage${search}`))
  }

  /** Connectivity + auth test against the saved configuration. */
  async test(): Promise<NyroTestResult> {
    return readJson<NyroTestResult>(await fetch('/api/nyro-usage/test', { method: 'POST' }))
  }
}
