/**
 * dsh-nyro-usage — host half. Registers the loopback-fenced
 * /api/nyro-usage route family that proxies the configured nyro gateway's
 * admin plane (bulk provider usage / status / connection test) and installs
 * the `nyro-usage` settings section the web settings card edits. The
 * browser half (./client) renders the sidebar entry and the usage panel.
 * Everything rides official NPM SDK packages — no dsh source changes.
 */

import type { Context } from '@deepseek-ai/cordis'
import { installSettingsSection, settingsNamespace } from '@deepseek-ai/dsh-settings'
import z from 'schemastery'
import type {} from '@deepseek-ai/dsh-host-webserver'
import { makeRoutes, type NyroUsageRouteConfig } from './routes.ts'
import { normalizeBaseUrl } from './nyro.ts'

/** Stable cordis plugin name. */
export const name = 'nyro-usage'

/** Services required before the proxy routes can mount. */
export const inject = ['webServer']

/**
 * Settings namespace of the nyro-usage capability — the section the web
 * settings surface edits. Spelled here rather than imported: the browser
 * half spells the same value and must not depend on a Host package.
 */
export const NYRO_USAGE_SETTINGS_NAMESPACE = settingsNamespace('nyro-usage')

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

export const Config: z<Config> = z.object({
  enabled: z.boolean().default(true),
  baseUrl: z.string().default(''),
  adminToken: z.string().role('secret').default(''),
  refreshSeconds: z.number().min(15).default(300),
  cacheSeconds: z.number().min(0).default(30),
})

/**
 * Mount the nyro usage proxy routes.
 * @param ctx - host plugin context carrying webServer.
 * @param config - resolved plugin config (schema defaults applied by the loader).
 */
export function apply(ctx: Context, config: Config = {}): void {
  // The live source the surfaces read: the settings section once the web
  // settings surface is served, the composition entry otherwise.
  let current: () => Config = () => config ?? {}
  const resolve = (): NyroUsageRouteConfig => {
    const value = current()
    return {
      baseUrl: normalizeBaseUrl(value.baseUrl),
      adminToken: value.adminToken ?? '',
      refreshSeconds: value.refreshSeconds ?? 300,
      cacheSeconds: value.cacheSeconds ?? 30,
    }
  }

  let disposeRoutes: (() => void) | undefined

  // Register (or drop) the routes to match the current source; one disposer
  // for the whole family so re-registering never trips the webserver's
  // duplicate-route guard.
  const sync = (): void => {
    if (disposeRoutes !== undefined) {
      disposeRoutes()
      disposeRoutes = undefined
    }
    if ((current().enabled ?? true) === false) return
    const routes = makeRoutes({ config: resolve })
    const disposers = routes.map(route => ctx.webServer.register(route))
    disposeRoutes = () => { for (const dispose of disposers) dispose() }
    ctx.effect(() => () => { disposeRoutes?.() }, 'nyro-usage: routes')
  }

  installSettingsSection(ctx, NYRO_USAGE_SETTINGS_NAMESPACE, Config, config ?? {}, {
    setSource: (source) => { current = source },
    onChange: sync,
  })
  sync()
}
