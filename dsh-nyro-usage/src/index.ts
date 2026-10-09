/**
 * dsh-nyro-usage — host half. Registers the loopback-fenced
 * /api/nyro-usage route family that proxies the configured nyro gateway's
 * admin plane (bulk provider usage / status / connection test); the
 * `nyro-usage` settings section is served by the module-level `Config`
 * export (dsh ≥ 0.1.7: namespace = profile entry id, volatile fields are
 * live-editable without remounting this fiber). The browser half
 * (./client) renders the sidebar entry and the usage panel. Everything
 * rides official NPM SDK packages — no dsh source changes.
 */

import type { Context } from '@deepseek-ai/cordis'
import './schemastery-volatile.d.ts'
import { derefVolatile } from './volatile.ts'
import z from 'schemastery'
import type {} from '@deepseek-ai/dsh-host-webserver'
import { makeRoutes, type NyroUsageRouteConfig } from './routes.ts'
import { normalizeBaseUrl } from './nyro.ts'

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

/** Plugin config, validated by the same-named schemastery schema. */
export interface Config {
  /** Master switch for the plugin (proxy routes). */
  enabled?: boolean
  /** nyro admin-plane base URL, e.g. `http://192.168.31.2:19531` (a
   * trailing `/api/v1` is tolerated and stripped). */
  baseUrl?: string
  /** nyro admin token (`Authorization: Bearer …`); secret — redacted on
   * settings read-back. */
  adminToken?: string
  /** Panel auto-refresh interval in seconds. */
  refreshSeconds?: number
  /** Host-side cache TTL for the bulk usage call in seconds (nyro queries
   * its upstreams live on every call, so the cache keeps repeated panel
   * loads polite). 0 disables caching. */
  cacheSeconds?: number
}

/**
 * Module-level Config: dsh ≥ 0.1.7 derives the settings section from this
 * export (namespace = entry id). Fields are marked volatile through
 * `.extra('volatile', true)` so they stay live-editable; because plain
 * `schemastery` does not resolve volatile fields to references, values
 * arrive as plain scalars and `derefVolatile` below keeps reads correct
 * under both plain and forked schema resolutions.
 */
export const Config: z<Config> = z.object({
  enabled: z.boolean().default(true).extra('volatile', true),
  baseUrl: z.string().default('').extra('volatile', true),
  adminToken: z.string().role('secret').default('').extra('volatile', true),
  refreshSeconds: z.number().min(15).default(300).extra('volatile', true),
  cacheSeconds: z.number().min(0).default(30).extra('volatile', true),
})

/**
 * Mount the nyro usage proxy routes.
 * @param ctx - host plugin context carrying webServer.
 * @param config - resolved plugin config (schema defaults applied by the loader).
 */
export function apply(ctx: Context, config: Config = {}): void {
  // The live source the surfaces read: the composition entry (settings
  // writes land through the config editor's volatile-update path).
  const current: () => Config = () => config ?? {}
  const resolve = (): NyroUsageRouteConfig => {
    const value = current()
    return {
      baseUrl: normalizeBaseUrl(derefVolatile(value.baseUrl, '')),
      adminToken: derefVolatile(value.adminToken, ''),
      refreshSeconds: derefVolatile(value.refreshSeconds, 300),
      cacheSeconds: derefVolatile(value.cacheSeconds, 30),
    }
  }

  const sync = (): void => {
    if (ctx.webServer === undefined) return
    if (derefVolatile<boolean>(current().enabled, true) === false) return
    const routes = makeRoutes({ config: resolve })
    const disposers = routes.map(route => ctx.webServer.register(route))
    ctx.effect(() => () => { for (const dispose of disposers) dispose() }, 'nyro-usage: routes')
  }
  sync()
}
