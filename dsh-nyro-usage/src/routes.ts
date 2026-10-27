/**
 * The /api/nyro-usage route family: a loopback-fenced proxy in front of the
 * configured nyro gateways' admin planes. The browser half fetches these
 * same-origin routes — no nyro base URL needs CORS openings and no admin
 * token ever leaves the host process.
 *
 * Routes (all loopback-only, mirroring the dsh-ssh trust fence):
 * - GET  /api/nyro-usage/usage?instance=<id>&refresh=1 — bulk provider usage
 *   per gateway (TTL-cached per gateway; `refresh=1` bypasses the cache;
 *   without `instance` every gateway is queried concurrently). One gateway's
 *   failure is reported in its own slice and never fails the others.
 * - GET  /api/nyro-usage/status — sanitized config + per-gateway cache age
 *   (token presence only, never a token).
 * - POST /api/nyro-usage/test?instance=<id> — connectivity + auth test
 *   against the saved configuration (every gateway without `instance`).
 */

import type { IncomingMessage, ServerResponse } from 'node:http'
import type { WebRoute } from '@deepseek-ai/dsh-host-webserver'
import type { ResolvedInstance, TokenFlags } from './instances.ts'
import {
  NyroApiError,
  NyroClient,
  type NyroInstanceStatus,
  type NyroInstanceTestResult,
  type NyroInstanceUsage,
  type NyroStatusResponse,
  type NyroTestRouteResponse,
  type NyroUsageResponse,
  type NyroUsageRouteResponse,
} from './nyro.ts'

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

/** What the plugin currently resolves from its settings. */
export interface NyroUsageRouteConfig {
  /** Plugin master switch. */
  enabled: boolean
  /** Gateways to query, primary first. */
  instances: ResolvedInstance[]
  /** Token presence by config position (settings card read-back). */
  tokens: TokenFlags
  refreshSeconds: number
  cacheSeconds: number
}

/** A cached bulk-usage snapshot. */
interface Snapshot {
  at: number
  data: NyroUsageResponse
}

/** Cached bulk-usage snapshot of ONE gateway with in-flight dedupe. */
class UsageCache {
  private readonly instance: ResolvedInstance
  private entry: Snapshot | undefined
  private inflight: Promise<Snapshot> | undefined

  constructor(instance: ResolvedInstance) {
    this.instance = instance
  }

  /**
   * Read the bulk usage, serving the cached copy while it is fresh enough.
   * Concurrent readers share one upstream call.
   * @param ttlSeconds - cache TTL (0 disables caching).
   * @param force - bypass the cache (manual refresh).
   * @returns the snapshot plus its fetch time.
   */
  read(ttlSeconds: number, force: boolean): Promise<Snapshot> {
    const ttl = Math.max(0, ttlSeconds) * 1000
    if (!force && this.entry !== undefined && Date.now() - this.entry.at < ttl) {
      return Promise.resolve(this.entry)
    }
    this.inflight ??= new NyroClient(this.instance.baseUrl, this.instance.adminToken)
      .usage()
      .then((data) => {
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
}

/** Cache identity: a gateway whose URL or token changed must not serve the
 * previous gateway's data. */
function signatureOf(instance: ResolvedInstance): string {
  return `${instance.id}\u0000${instance.baseUrl}\u0000${instance.adminToken}`
}

/** Error text + kind of anything a gateway call threw. */
function describeError(error: unknown): { message: string; kind: string } {
  if (error instanceof NyroApiError) return { message: error.message, kind: error.kind }
  return { message: error instanceof Error ? error.message : String(error), kind: 'network' }
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
  /** One cache per gateway signature; stale signatures are pruned per request. */
  const caches = new Map<string, UsageCache>()

  const current = (): NyroUsageRouteConfig => {
    const value = deps.config()
    const live = new Set(value.instances.map(signatureOf))
    for (const key of caches.keys()) {
      if (!live.has(key)) caches.delete(key)
    }
    return value
  }

  const cacheOf = (instance: ResolvedInstance): UsageCache => {
    const key = signatureOf(instance)
    let cache = caches.get(key)
    if (cache === undefined) {
      cache = new UsageCache(instance)
      caches.set(key, cache)
    }
    return cache
  }

  /** Narrow to `?instance=<id>` when given; undefined = unknown id. */
  const targetsOf = (url: URL, value: NyroUsageRouteConfig): ResolvedInstance[] | undefined => {
    const wanted = url.searchParams.get('instance')
    if (wanted === null || wanted === '') return value.instances
    const match = value.instances.find(instance => instance.id === wanted)
    return match === undefined ? undefined : [match]
  }

  const guard = (req: IncomingMessage, res: ServerResponse, method: string): URL | undefined => {
    if (!isLoopbackRequest(req)) {
      writeJson(res, 403, { error: 'forbidden: loopback-only' })
      return undefined
    }
    if (req.method !== method) {
      writeJson(res, 405, { error: `method not allowed: ${req.method}` })
      return undefined
    }
    return new URL(req.url ?? '/', 'http://localhost')
  }

  const readInstance = async (instance: ResolvedInstance, ttl: number, force: boolean): Promise<NyroInstanceUsage> => {
    const head = { id: instance.id, name: instance.name, baseUrl: instance.baseUrl }
    if (instance.adminToken === '') {
      return { ...head, items: [], cachedAt: null, error: 'adminToken is not configured', errorKind: 'unconfigured' }
    }
    try {
      const snapshot = await cacheOf(instance).read(ttl, force)
      return { ...head, items: snapshot.data.data ?? [], cachedAt: new Date(snapshot.at).toISOString(), error: null, errorKind: null }
    } catch (error) {
      const { message, kind } = describeError(error)
      return { ...head, items: [], cachedAt: null, error: message, errorKind: kind }
    }
  }

  const testInstance = async (instance: ResolvedInstance): Promise<NyroInstanceTestResult> => {
    const head = { id: instance.id, name: instance.name, baseUrl: instance.baseUrl }
    try {
      return { ...head, ...await new NyroClient(instance.baseUrl, instance.adminToken).test() }
    } catch (error) {
      const { message, kind } = describeError(error)
      return { ...head, ok: false, kind, message }
    }
  }

  const usageRoute: WebRoute = {
    kind: 'exact',
    path: NYRO_API.usage,
    handler: async (req, res) => {
      const url = guard(req, res, 'GET')
      if (url === undefined) return
      const value = current()
      if (!value.enabled) {
        writeJson(res, 503, { error: 'nyro-usage plugin is disabled', kind: 'disabled' })
        return
      }
      if (value.instances.length === 0) {
        writeJson(res, 400, { error: 'no nyro gateway configured (baseUrl / instances)', kind: 'unconfigured' })
        return
      }
      const targets = targetsOf(url, value)
      if (targets === undefined) {
        writeJson(res, 404, { error: 'unknown nyro instance', kind: 'not-found' })
        return
      }
      const force = url.searchParams.get('refresh') === '1'
      const body: NyroUsageRouteResponse = {
        instances: await Promise.all(targets.map(instance => readInstance(instance, value.cacheSeconds, force))),
      }
      writeJson(res, 200, body)
    },
  }

  const statusRoute: WebRoute = {
    kind: 'exact',
    path: NYRO_API.status,
    handler: (req, res) => {
      if (guard(req, res, 'GET') === undefined) return
      const value = current()
      const instances: NyroInstanceStatus[] = value.instances.map((instance) => {
        const at = caches.get(signatureOf(instance))?.cachedAt()
        return {
          id: instance.id,
          name: instance.name,
          baseUrl: instance.baseUrl,
          hasToken: instance.adminToken !== '',
          cachedAt: at === undefined ? null : new Date(at).toISOString(),
        }
      })
      const body: NyroStatusResponse = {
        configured: value.enabled && instances.length > 0,
        disabled: !value.enabled,
        refreshSeconds: value.refreshSeconds,
        cacheSeconds: value.cacheSeconds,
        instances,
        tokens: { primary: value.tokens.primary, rows: [...value.tokens.rows] },
      }
      writeJson(res, 200, body)
    },
  }

  const testRoute: WebRoute = {
    kind: 'exact',
    path: NYRO_API.test,
    handler: async (req, res) => {
      const url = guard(req, res, 'POST')
      if (url === undefined) return
      const value = current()
      if (!value.enabled) {
        writeJson(res, 200, { ok: false, kind: 'disabled', message: 'nyro-usage plugin is disabled', instances: [] } satisfies NyroTestRouteResponse)
        return
      }
      const targets = targetsOf(url, value)
      if (targets === undefined || targets.length === 0) {
        writeJson(res, 200, {
          ok: false,
          kind: targets === undefined ? 'not-found' : 'unconfigured',
          message: targets === undefined ? 'unknown nyro instance' : 'no nyro gateway configured (baseUrl / instances)',
          instances: [],
        } satisfies NyroTestRouteResponse)
        return
      }
      const instances = await Promise.all(targets.map(testInstance))
      const failed = instances.find(result => !result.ok)
      const body: NyroTestRouteResponse = failed === undefined
        ? {
            ok: true,
            kind: 'reachable',
            message: 'connected',
            providerCount: instances.reduce((sum, result) => sum + (result.providerCount ?? 0), 0),
            instances,
          }
        : { ok: false, kind: failed.kind, message: `${failed.name}: ${failed.message}`, instances }
      writeJson(res, 200, body)
    },
  }

  return [usageRoute, statusRoute, testRoute]
}
