/**
 * dsh-nyro-usage — host half. Registers the loopback-fenced
 * /api/nyro-usage route family that proxies the configured nyro gateways'
 * admin planes (bulk provider usage / status / connection test); the
 * `nyro-usage` settings section is served by the module-level `Config`
 * export (dsh ≥ 0.1.7: namespace = profile entry id, volatile fields are
 * live-editable without remounting this fiber). The browser half
 * (./client) renders the sidebar entry and the usage panel. Everything
 * rides official NPM SDK packages — no dsh source changes.
 *
 * Several gateways are supported: the top-level `baseUrl` / `adminToken`
 * is the primary gateway (existing single-gateway configs keep working),
 * `instances[]` lists the additional ones.
 */

import type { Context } from '@deepseek-ai/cordis'
import z from '@deepseek-ai/schemastery'
import type {} from '@deepseek-ai/dsh-host-webserver'
import { resolveInstances, tokenFlags, type NyroInstanceConfig } from './instances.ts'
import { makeRoutes, type NyroUsageRouteConfig } from './routes.ts'
import { derefVolatile } from './volatile.ts'

/** Stable cordis plugin name. */
export const name = 'nyro-usage'

/** Services required before the proxy routes can mount. */
export const inject = ['webServer']

/**
 * Settings namespace of the nyro-usage capability — the profile entry id
 * (the dsh ≥ 0.1.7 settings key). Spelled here rather than derived: the
 * browser half spells the same value and must not depend on a Host package.
 */
export const NYRO_USAGE_SETTINGS_NAMESPACE = 'nyro-usage'

export type { NyroInstanceConfig }

/** Plugin config, validated by the same-named schemastery schema. */
export interface Config {
  /** Master switch for the plugin (proxy routes). */
  enabled?: boolean
  /** Display name of the primary gateway (defaults to its URL host). */
  name?: string
  /** Primary nyro admin-plane base URL, e.g. `http://192.168.31.2:19531`
   * (a trailing `/api/v1` is tolerated and stripped). */
  baseUrl?: string
  /** Primary nyro admin token (`Authorization: Bearer …`); secret —
   * redacted on settings read-back. */
  adminToken?: string
  /** Additional nyro gateways, rendered after the primary one. */
  instances?: NyroInstanceConfig[]
  /** Panel auto-refresh interval in seconds. */
  refreshSeconds?: number
  /** Host-side cache TTL for the bulk usage call in seconds (nyro queries
   * its upstreams live on every call, so the cache keeps repeated panel
   * loads polite). 0 disables caching. */
  cacheSeconds?: number
}

/** Schema of one `instances[]` row. */
const InstanceSchema = z.object({
  name: z.string().default(''),
  baseUrl: z.string().default(''),
  adminToken: z.string().role('secret').default(''),
  enabled: z.boolean().default(true),
})

/**
 * Module-level Config: dsh ≥ 0.1.7 derives the settings section from this
 * export (namespace = entry id). Every field is volatile: the
 * `@deepseek-ai/schemastery` fork resolves volatile fields to references the
 * loader updates in place on a settings write, so edits apply without
 * remounting the fiber; reads go through `derefVolatile`.
 */
export const Config: z<Config> = z.object({
  enabled: z.boolean().default(true).extra('volatile', true),
  name: z.string().default('').extra('volatile', true),
  baseUrl: z.string().default('').extra('volatile', true),
  adminToken: z.string().role('secret').default('').extra('volatile', true),
  instances: z.array(InstanceSchema).default([]).extra('volatile', true),
  refreshSeconds: z.number().min(15).default(300).extra('volatile', true),
  cacheSeconds: z.number().min(0).default(30).extra('volatile', true),
})

/**
 * Resolve the route config from the plugin config (scalars or volatile refs).
 * @param value - the resolved plugin config.
 * @returns what the routes read per request.
 */
export function resolveRouteConfig(value: Config): NyroUsageRouteConfig {
  return {
    enabled: derefVolatile<boolean>(value.enabled, true) !== false,
    instances: resolveInstances(value),
    tokens: tokenFlags(value),
    refreshSeconds: derefVolatile(value.refreshSeconds, 300),
    cacheSeconds: derefVolatile(value.cacheSeconds, 30),
  }
}

/**
 * Mount the nyro usage proxy routes.
 * @param ctx - host plugin context carrying webServer.
 * @param config - resolved plugin config (schema defaults applied by the loader).
 */
export function apply(ctx: Context, config: Config = {}): void {
  if (ctx.webServer === undefined) return
  // The volatile references inside `config` are updated in place by the
  // loader on every settings write; re-reading them per request applies
  // edits (including the gateway list and the master switch) live.
  const routes = makeRoutes({ config: () => resolveRouteConfig(config ?? {}) })
  const disposers = routes.map(route => ctx.webServer.register(route))
  ctx.effect(() => () => { for (const dispose of disposers) dispose() }, 'nyro-usage: routes')
}
