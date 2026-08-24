/**
 * The /api/nyro-usage route family: a loopback-fenced proxy in front of the
 * configured nyro gateway's admin plane. The browser half fetches these
 * same-origin routes — the nyro base URL never needs CORS openings and the
 * admin token never leaves the host process.
 *
 * Routes (all loopback-only, mirroring the dsh-ssh trust fence):
 * - GET  /api/nyro-usage/usage?refresh=1 — bulk provider usage (TTL-cached;
 *   `refresh=1` bypasses the cache). nyro queries its upstreams live, so the
 *   cache keeps sidebar badges / auto-refresh / multiple tabs polite.
 * - GET  /api/nyro-usage/status — sanitized config + cache age (no token).
 * - POST /api/nyro-usage/test — connectivity + auth test against the saved
 *   configuration.
 */

import type { IncomingMessage, ServerResponse } from 'node:http'
import type { WebRoute } from '@deepseek-ai/dsh-host-webserver'
import { NyroApiError, NyroClient, normalizeBaseUrl, type NyroStatusResponse, type NyroTestResult, type NyroUsageResponse } from './nyro.ts'
export type { NyroStatusResponse }

/** Route paths this family owns. */
export const NYRO_API = {
  usage: '/api/nyro-usage/usage',
  status: '/api/nyro-usage/status',
  test: '/api/nyro-usage/test',
} as const

/** Loopback literal check plus browser same-origin markers (mirrors the
 * dsh-ssh pairing-routes fence: LAN-exposed dsh web deployments must not
 * serve the usage proxy to remote callers). */
function isLoopbackRequest(request: IncomingMessage): boolean {
  const address = request.socket.remoteAddress
  if (address !== '127.0.0.1' && address !== '::1' && address !== '::ffff:127.0.0.1') return false
  const host = request.headers.host
  if (typeof host !== 'string') return false
  let hostUrl: URL
  try {
    hostUrl = new URL(`http://${host}`)
  } catch {
    return false
  }
  if (hostUrl.hostname !== '127.0.0.1' && hostUrl.hostname !== 'localhost' && hostUrl.hostname !== '[::1]') return false
  if (request.headers['sec-fetch-site'] === 'cross-site') return false
  const origin = request.headers.origin
  if (origin === undefined) return true
  try {
    return new URL(origin).host === hostUrl.host
  } catch {
    return false
  }
}

/** One JSON response. */
function writeJson(res: ServerResponse, status: number, body: unknown): void {
  const payload = JSON.stringify(body)
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8', 'referrer-policy': 'no-referrer' })
  res.end(payload)
}

/** Map a NyroApiError to an HTTP status the panel can branch on. */
function errorStatus(kind: string): number {
  switch (kind) {
    case 'unconfigured': return 400
    case 'bad-url': return 400
    case 'unauthorized': return 401
    case 'not-found': return 501
    case 'network': return 502
    default: return 502
  }
}

/** What the plugin currently resolves from its settings. */
export interface NyroUsageRouteConfig {
  baseUrl: string
  adminToken: string
  refreshSeconds: number
  cacheSeconds: number
}

/** Cached bulk-usage snapshot with in-flight dedupe. */
class UsageCache {
  private entry: { at: number; data: NyroUsageResponse } | undefined
  private inflight: Promise<{ at: number; data: NyroUsageResponse }> | undefined

  /**
   * Read the bulk usage, serving the cached copy while it is fresh enough.
   * Concurrent readers share one upstream call.
   * @param config - resolved plugin config (client + TTL).
   * @param force - bypass the cache (manual refresh).
   * @returns the snapshot plus its fetch time.
   */
  read(config: NyroUsageRouteConfig, force: boolean): Promise<{ at: number; data: NyroUsageResponse }> {
    const ttl = Math.max(0, config.cacheSeconds) * 1000
    if (!force && this.entry !== undefined && Date.now() - this.entry.at < ttl) {
      return Promise.resolve(this.entry)
    }
    this.inflight ??= new NyroClient(config.baseUrl, config.adminToken)
      .usage()
      .then(data => {
        const snapshot = { at: Date.now(), data }
        this.entry = snapshot
        return snapshot
      })
      .finally(() => { this.inflight = undefined })
    return this.inflight
  }

  /** The cached fetch time, if any (for the status route). */
  cachedAt(): number | undefined {
    return this.entry?.at
  }

  /** Drop the cache (config change: the token/baseUrl may have changed). */
  invalidate(): void {
    this.entry = undefined
  }
}

/** Route family dependencies. */
export interface NyroUsageRoutesDeps {
  /** Reads the currently resolved config (called per request). */
  config: () => NyroUsageRouteConfig
}

/**
 * Build the /api/nyro-usage route family.
 * @param deps - config source.
 * @returns the routes to register on the webserver.
 */
export function makeRoutes(deps: NyroUsageRoutesDeps): WebRoute[] {
  const cache = new UsageCache()
  // A config change must not serve another gateway's cached data.
  let lastSignature = ''
  const signature = (): string => {
    const value = deps.config()
    return `${value.baseUrl}\u0000${value.adminToken}`
  }

  const clientOf = (): NyroClient => {
    const value = deps.config()
    const next = signature()
    if (next !== lastSignature) {
      cache.invalidate()
      lastSignature = next
    }
    return new NyroClient(value.baseUrl, value.adminToken)
  }

  const guard = (req: IncomingMessage, res: ServerResponse): boolean => {
    if (!isLoopbackRequest(req)) {
      writeJson(res, 403, { error: 'forbidden: loopback-only' })
      return false
    }
    return true
  }

  const usageRoute: WebRoute = {
    kind: 'exact',
    path: NYRO_API.usage,
    handler: async (req, res) => {
      if (!guard(req, res)) return
      if (req.method !== 'GET') {
        writeJson(res, 405, { error: `method not allowed: ${req.method}` })
        return
      }
      const value = deps.config()
      if (value.baseUrl === '' || value.adminToken.trim() === '') {
        writeJson(res, 400, { error: 'nyro baseUrl / adminToken not configured' })
        return
      }
      const url = new URL(req.url ?? '/', 'http://localhost')
      const force = url.searchParams.get('refresh') === '1'
      try {
        clientOf() // signature check + cache invalidate on config change
        const snapshot = await cache.read(value, force)
        // Flatten nyro's own `{data: [...]}` envelope: the panel contract is
        // `{data: items, cachedAt}`.
        writeJson(res, 200, { data: snapshot.data.data ?? [], cachedAt: new Date(snapshot.at).toISOString() })
      } catch (error) {
        if (error instanceof NyroApiError) {
          writeJson(res, errorStatus(error.kind), { error: error.message, kind: error.kind })
          return
        }
        writeJson(res, 502, { error: error instanceof Error ? error.message : String(error) })
      }
    },
  }

  const statusRoute: WebRoute = {
    kind: 'exact',
    path: NYRO_API.status,
    handler: (req, res) => {
      if (!guard(req, res)) return
      if (req.method !== 'GET') {
        writeJson(res, 405, { error: `method not allowed: ${req.method}` })
        return
      }
      const value = deps.config()
      const cachedAt = cache.cachedAt()
      const body: NyroStatusResponse = {
        configured: value.baseUrl !== '' && value.adminToken.trim() !== '',
        baseUrl: value.baseUrl,
        hasToken: value.adminToken.trim() !== '',
        refreshSeconds: value.refreshSeconds,
        cacheSeconds: value.cacheSeconds,
        cachedAt: cachedAt === undefined ? null : new Date(cachedAt).toISOString(),
      }
      writeJson(res, 200, body)
    },
  }

  const testRoute: WebRoute = {
    kind: 'exact',
    path: NYRO_API.test,
    handler: async (req, res) => {
      if (!guard(req, res)) return
      if (req.method !== 'POST') {
        writeJson(res, 405, { error: `method not allowed: ${req.method}` })
        return
      }
      try {
        const result: NyroTestResult = await clientOf().test()
        writeJson(res, 200, result)
      } catch (error) {
        if (error instanceof NyroApiError) {
          writeJson(res, 200, { ok: false, kind: error.kind, message: error.message } satisfies NyroTestResult)
          return
        }
        writeJson(res, 200, { ok: false, kind: 'network', message: error instanceof Error ? error.message : String(error) } satisfies NyroTestResult)
      }
    },
  }

  return [usageRoute, statusRoute, testRoute]
}
