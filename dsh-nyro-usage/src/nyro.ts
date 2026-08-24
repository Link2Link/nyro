/**
 * nyro Admin API client (host side).
 *
 * Talks to the configured nyro gateway's admin plane
 * (`GET {baseUrl}/api/v1/...` with `Authorization: Bearer <adminToken>`) and
 * normalizes the provider-usage surface the panel renders:
 * - `GET /api/v1/providers/usage` — every provider's upstream quota/balance
 *   usage in one call (nyro queries its upstreams concurrently; the call
 *   takes seconds, hence the generous timeout).
 * - `GET /api/v1/providers` — cheap authenticated call used by the
 *   connection test.
 */

/** One quota window (e.g. 5-hour rolling, weekly, monthly). */
export interface NyroTier {
  name: string
  used_percent: number
  resets_at?: string | null
}

/** Pay-as-you-go account balance, one entry per currency (DeepSeek shape). */
export interface NyroBalance {
  currency: string
  total: number
  granted: number
  topped_up: number
}

/** A spend figure over a period (`today` | `month`). */
export interface NyroSpend {
  name: string
  amount: number
  currency: string
}

/** Runtime scheduling decision derived from the usage snapshot. */
export interface NyroScheduling {
  status?: string
  reason?: string | null
  blocking_tiers?: string[]
  reset_at?: string | null
  next_check_at?: string | null
}

/** Full usage payload of one provider. */
export interface NyroUsage {
  provider_id: string
  kind: string
  site: string
  level?: string | null
  tiers?: NyroTier[]
  balances?: NyroBalance[]
  spends?: NyroSpend[]
  is_available?: boolean | null
  scheduling?: NyroScheduling
  queried_at?: string
}

/** One provider row of the bulk usage listing. */
export interface NyroUsageItem {
  provider_id: string
  provider_name: string
  is_enabled: boolean
  status: 'ok' | 'unsupported' | 'error'
  error?: string | null
  usage?: NyroUsage | null
}

/** Bulk endpoint body: `{ "data": [ ... ] }`. */
export interface NyroUsageResponse {
  data: NyroUsageItem[]
}

/** Connection test outcome. */
export interface NyroTestResult {
  ok: boolean
  /** `reachable` | `unauthorized` | `not-found` | `network` | `unconfigured` | `bad-url`. */
  kind: string
  message: string
  providerCount?: number
}

/**
 * Normalize a configured base URL: trims whitespace, drops trailing slashes
 * and a trailing `/api/v1` (operators paste either shape). Returns '' when
 * nothing usable remains.
 */
export function normalizeBaseUrl(raw: string | undefined): string {
  const trimmed = (raw ?? '').trim()
  if (trimmed === '') return ''
  let url = trimmed.replace(/\/+$/, '')
  if (url.endsWith('/api/v1')) url = url.slice(0, -'/api/v1'.length).replace(/\/+$/, '')
  return url
}

/** What the plugin currently resolves from its settings (sanitized form the
 * browser may see — never the token itself). */
export interface NyroStatusResponse {
  configured: boolean
  baseUrl: string
  hasToken: boolean
  refreshSeconds: number
  cacheSeconds: number
  cachedAt: string | null
}

/** Errors carrying a machine-readable classification for route mapping. */
export class NyroApiError extends Error {
  constructor(
    message: string,
    /** `unauthorized` | `not-found` | `network` | `bad-url` | `unconfigured` | `server`. */
    readonly kind: string,
  ) {
    super(message)
    this.name = 'NyroApiError'
  }
}

/** Fetch timeout: nyro's bulk usage queries upstreams concurrently (4-way)
 * with per-request timeouts of 15–20s, so the whole call can legitimately
 * take tens of seconds. */
const USAGE_TIMEOUT_MS = 70_000
const TEST_TIMEOUT_MS = 10_000

/** Reject non-http(s) schemes early (file:, etc. must never be fetched). */
function assertHttpUrl(base: string): void {
  let parsed: URL
  try {
    parsed = new URL(base)
  } catch {
    throw new NyroApiError(`invalid baseUrl: ${base}`, 'bad-url')
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    throw new NyroApiError(`baseUrl must be http(s): ${base}`, 'bad-url')
  }
}

/** Shared fetch + error classification for the nyro admin plane. */
async function nyroFetch(base: string, token: string, path: string, timeoutMs: number): Promise<unknown> {
  if (base === '') throw new NyroApiError('baseUrl is not configured', 'unconfigured')
  if (token.trim() === '') throw new NyroApiError('adminToken is not configured', 'unconfigured')
  assertHttpUrl(base)
  let response: Response
  try {
    response = await fetch(`${base}${path}`, {
      headers: { authorization: `Bearer ${token}` },
      signal: AbortSignal.timeout(timeoutMs),
    })
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error)
    const timedOut = error instanceof Error && error.name === 'TimeoutError'
    throw new NyroApiError(
      timedOut ? `nyro unreachable (timeout): ${base}` : `nyro unreachable: ${reason}`,
      'network',
    )
  }
  if (response.status === 401) {
    throw new NyroApiError('invalid admin token (HTTP 401)', 'unauthorized')
  }
  let body: unknown
  try {
    body = await response.json()
  } catch {
    body = undefined
  }
  const bodyError = typeof body === 'object' && body !== null
    ? (body as { error?: unknown }).error
    : undefined
  // Old nyro builds route GET /providers/usage to GET /providers/:id and
  // answer 200 with {"error":"provider not found: usage"}. Both that shape
  // and a plain 404 mean: bulk usage endpoint missing → upgrade nyro.
  const notFound = response.status === 404
    || (typeof bodyError === 'string' && bodyError.includes('provider not found: usage'))
  if (notFound) {
    throw new NyroApiError(
      'nyro has no GET /api/v1/providers/usage endpoint (version too old); upgrade nyro',
      'not-found',
    )
  }
  if (!response.ok) {
    const detail = typeof bodyError === 'string' && bodyError !== '' ? bodyError : `HTTP ${response.status}`
    throw new NyroApiError(`nyro error: ${detail}`, 'server')
  }
  return body
}

/** Client for one configured nyro gateway. Cheap to construct per request. */
export class NyroClient {
  constructor(
    private readonly baseUrl: string,
    private readonly adminToken: string,
  ) {}

  /** Fetch every provider's usage (the bulk endpoint). */
  async usage(): Promise<NyroUsageResponse> {
    const body = await nyroFetch(this.baseUrl, this.adminToken, '/api/v1/providers/usage', USAGE_TIMEOUT_MS)
    if (typeof body !== 'object' || body === null || !Array.isArray((body as { data?: unknown }).data)) {
      throw new NyroApiError('unexpected nyro response: missing data array', 'server')
    }
    return body as NyroUsageResponse
  }

  /** Cheap authenticated call used by the connection test. */
  async test(): Promise<NyroTestResult> {
    const body = await nyroFetch(this.baseUrl, this.adminToken, '/api/v1/providers', TEST_TIMEOUT_MS)
    const count = typeof body === 'object' && body !== null && Array.isArray((body as { data?: unknown }).data)
      ? (body as { data: unknown[] }).data.length
      : undefined
    return {
      ok: true,
      kind: 'reachable',
      message: 'connected',
      ...count === undefined ? {} : { providerCount: count },
    }
  }
}
