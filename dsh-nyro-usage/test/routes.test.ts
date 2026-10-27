import assert from 'node:assert/strict'
import type { IncomingMessage, ServerResponse } from 'node:http'
import { afterEach, beforeEach, describe, it } from 'node:test'
import type { WebRoute } from '@deepseek-ai/dsh-host-webserver'
import { resolveInstances, tokenFlags, type InstanceSource } from '../src/instances.ts'
import { makeRoutes, NYRO_API, type NyroUsageRouteConfig } from '../src/routes.ts'

/** Route config from a raw source, as index.ts builds it. */
function routeConfig(source: InstanceSource & { enabled?: boolean; cacheSeconds?: number }): NyroUsageRouteConfig {
  return {
    enabled: source.enabled !== false,
    instances: resolveInstances(source),
    tokens: tokenFlags(source),
    refreshSeconds: 300,
    cacheSeconds: source.cacheSeconds ?? 30,
  }
}

interface Reply {
  status: number
  body: any
}

/** Call one route handler with a loopback request. */
async function call(route: WebRoute, method: string, url: string, address = '127.0.0.1'): Promise<Reply> {
  const req = {
    method,
    url,
    headers: { host: '127.0.0.1:3080' },
    socket: { remoteAddress: address },
  } as unknown as IncomingMessage
  let status = 0
  let payload = ''
  const res = {
    writeHead(code: number) { status = code; return res },
    end(chunk?: string) { payload = chunk ?? '' },
  } as unknown as ServerResponse
  await route.handler(req, res)
  return { status, body: payload === '' ? undefined : JSON.parse(payload) }
}

/** Index the route family by path. */
function routesOf(config: () => NyroUsageRouteConfig): Record<string, WebRoute> {
  return Object.fromEntries(makeRoutes({ config }).map(route => [route.path, route]))
}

const realFetch = globalThis.fetch
let calls: string[] = []
/** base URL → handler; a missing base answers like an unreachable host. */
let upstream: Record<string, (path: string, token: string) => Response> = {}

beforeEach(() => {
  calls = []
  upstream = {}
  globalThis.fetch = (async (input: string | URL, init?: RequestInit) => {
    const url = new URL(String(input))
    const base = `${url.protocol}//${url.host}`
    calls.push(`${base}${url.pathname}`)
    const token = String((init?.headers as Record<string, string>).authorization ?? '').replace(/^Bearer /, '')
    const handler = upstream[base]
    if (handler === undefined) throw new TypeError('fetch failed')
    return handler(url.pathname, token)
  }) as typeof fetch
})

afterEach(() => {
  globalThis.fetch = realFetch
})

/** A nyro gateway answering `providers` usage items with the given names. */
function gateway(names: string[], token: string): (path: string, given: string) => Response {
  return (path, given) => {
    if (given !== token) return new Response('{}', { status: 401 })
    const data = path.endsWith('/providers/usage')
      ? names.map(name => ({ provider_id: name, provider_name: name, is_enabled: true, status: 'ok' }))
      : names.map(name => ({ id: name }))
    return Response.json({ data })
  }
}

describe('usage route', () => {
  it('returns one slice per gateway and isolates a failing gateway', async () => {
    upstream['http://a:1'] = gateway(['A1', 'A2'], 'ta')
    upstream['http://b:2'] = gateway(['B1'], 'wrong')
    const routes = routesOf(() => routeConfig({
      baseUrl: 'http://a:1',
      adminToken: 'ta',
      instances: [
        { name: 'B', baseUrl: 'http://b:2', adminToken: 'tb' },
        { name: 'Down', baseUrl: 'http://down:3', adminToken: 'td' },
        { name: 'No token', baseUrl: 'http://n:4' },
      ],
    }))
    const reply = await call(routes[NYRO_API.usage], 'GET', NYRO_API.usage)
    assert.equal(reply.status, 200)
    const slices = reply.body.instances
    assert.deepEqual(slices.map((s: any) => [s.id, s.items.length, s.errorKind]), [
      ['default', 2, null],
      ['b', 0, 'unauthorized'],
      ['down', 0, 'network'],
      ['no-token', 0, 'unconfigured'],
    ])
    assert.equal(typeof slices[0].cachedAt, 'string')
    assert.equal(calls.includes('http://n:4/api/v1/providers/usage'), false, 'no request without a token')
  })

  it('narrows to ?instance= and rejects unknown ids', async () => {
    upstream['http://a:1'] = gateway(['A1'], 'ta')
    upstream['http://b:2'] = gateway(['B1'], 'tb')
    const routes = routesOf(() => routeConfig({
      baseUrl: 'http://a:1',
      adminToken: 'ta',
      instances: [{ name: 'B', baseUrl: 'http://b:2', adminToken: 'tb' }],
    }))
    const one = await call(routes[NYRO_API.usage], 'GET', `${NYRO_API.usage}?instance=b`)
    assert.deepEqual(one.body.instances.map((s: any) => s.items[0].provider_id), ['B1'])
    assert.deepEqual(calls, ['http://b:2/api/v1/providers/usage'])
    const missing = await call(routes[NYRO_API.usage], 'GET', `${NYRO_API.usage}?instance=zzz`)
    assert.equal(missing.status, 404)
  })

  it('caches per gateway and bypasses the cache on refresh=1', async () => {
    upstream['http://a:1'] = gateway(['A1'], 'ta')
    upstream['http://b:2'] = gateway(['B1'], 'tb')
    const routes = routesOf(() => routeConfig({
      baseUrl: 'http://a:1',
      adminToken: 'ta',
      instances: [{ name: 'B', baseUrl: 'http://b:2', adminToken: 'tb' }],
    }))
    await call(routes[NYRO_API.usage], 'GET', NYRO_API.usage)
    await call(routes[NYRO_API.usage], 'GET', NYRO_API.usage)
    assert.equal(calls.length, 2, 'second read served from cache')
    await call(routes[NYRO_API.usage], 'GET', `${NYRO_API.usage}?instance=b&refresh=1`)
    assert.deepEqual(calls.slice(2), ['http://b:2/api/v1/providers/usage'])
  })

  it('does not serve a gateway cache after its token changes', async () => {
    upstream['http://a:1'] = gateway(['A1'], 'new')
    let token = 'old'
    const routes = routesOf(() => routeConfig({ baseUrl: 'http://a:1', adminToken: token }))
    const before = await call(routes[NYRO_API.usage], 'GET', NYRO_API.usage)
    assert.equal(before.body.instances[0].errorKind, 'unauthorized')
    token = 'new'
    const after = await call(routes[NYRO_API.usage], 'GET', NYRO_API.usage)
    assert.equal(after.body.instances[0].items.length, 1)
  })

  it('answers 503 when disabled and 400 when nothing is configured', async () => {
    const disabled = routesOf(() => routeConfig({ enabled: false, baseUrl: 'http://a', adminToken: 't' }))
    assert.equal((await call(disabled[NYRO_API.usage], 'GET', NYRO_API.usage)).status, 503)
    const empty = routesOf(() => routeConfig({}))
    const reply = await call(empty[NYRO_API.usage], 'GET', NYRO_API.usage)
    assert.equal(reply.status, 400)
    assert.equal(reply.body.kind, 'unconfigured')
  })

  it('refuses non-loopback callers and wrong methods', async () => {
    const routes = routesOf(() => routeConfig({ baseUrl: 'http://a', adminToken: 't' }))
    assert.equal((await call(routes[NYRO_API.usage], 'GET', NYRO_API.usage, '192.168.1.9')).status, 403)
    assert.equal((await call(routes[NYRO_API.usage], 'POST', NYRO_API.usage)).status, 405)
    assert.deepEqual(calls, [])
  })
})

describe('status route', () => {
  it('lists gateways and token presence without leaking tokens', async () => {
    const routes = routesOf(() => routeConfig({
      baseUrl: 'http://a:1',
      adminToken: 'secret-a',
      instances: [
        { name: 'B', baseUrl: 'http://b:2', adminToken: 'secret-b' },
        { name: 'Off', baseUrl: 'http://c:3', adminToken: 'secret-c', enabled: false },
        { name: 'C', baseUrl: 'http://d:4' },
      ],
    }))
    const reply = await call(routes[NYRO_API.status], 'GET', NYRO_API.status)
    assert.equal(reply.status, 200)
    assert.equal(JSON.stringify(reply.body).includes('secret-'), false)
    assert.equal(reply.body.configured, true)
    assert.deepEqual(reply.body.instances.map((i: any) => [i.id, i.hasToken]), [['default', true], ['b', true], ['c', false]])
    assert.deepEqual(reply.body.tokens, { primary: true, rows: [true, true, false] })
  })

  it('still reports the configuration while disabled', async () => {
    const routes = routesOf(() => routeConfig({ enabled: false, baseUrl: 'http://a', adminToken: 't' }))
    const reply = await call(routes[NYRO_API.status], 'GET', NYRO_API.status)
    assert.equal(reply.body.configured, false)
    assert.equal(reply.body.disabled, true)
    assert.equal(reply.body.instances.length, 1)
  })
})

describe('test route', () => {
  it('tests every gateway and names the first failure', async () => {
    upstream['http://a:1'] = gateway(['A1', 'A2'], 'ta')
    upstream['http://b:2'] = gateway(['B1'], 'tb')
    const routes = routesOf(() => routeConfig({
      baseUrl: 'http://a:1',
      adminToken: 'ta',
      instances: [{ name: 'B', baseUrl: 'http://b:2', adminToken: 'tb' }, { name: 'Down', baseUrl: 'http://down:9', adminToken: 'x' }],
    }))
    const reply = await call(routes[NYRO_API.test], 'POST', NYRO_API.test)
    assert.equal(reply.body.ok, false)
    assert.match(reply.body.message, /^Down: /)
    assert.deepEqual(reply.body.instances.map((r: any) => [r.id, r.ok, r.providerCount ?? null]), [
      ['default', true, 2],
      ['b', true, 1],
      ['down', false, null],
    ])
    const one = await call(routes[NYRO_API.test], 'POST', `${NYRO_API.test}?instance=b`)
    assert.equal(one.body.ok, true)
    assert.equal(one.body.providerCount, 1)
  })
})
