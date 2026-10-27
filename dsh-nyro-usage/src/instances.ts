/**
 * Multi-gateway resolution: turns the plugin config into the ordered list of
 * nyro gateways the routes query.
 *
 * Two config shapes coexist, so existing single-gateway configs keep working
 * untouched:
 * - the primary gateway: the top-level `baseUrl` / `adminToken` (+ optional
 *   `name`) — always id `default`, listed first;
 * - additional gateways: the `instances[]` list, each
 *   `{ name, baseUrl, adminToken, enabled }`.
 *
 * Ids are URL/localStorage-safe slugs of the display name: the panel keys
 * sections, per-gateway card order and `?instance=` requests by them.
 */

import { normalizeBaseUrl } from './nyro.ts'
import { derefVolatile } from './volatile.ts'

/** Id of the primary (top-level) gateway. */
export const PRIMARY_INSTANCE_ID = 'default'

/** One `instances[]` row as configured. */
export interface NyroInstanceConfig {
  /** Display name; also seeds the stable id. Falls back to the URL host. */
  name?: string
  /** nyro admin-plane base URL (a trailing `/api/v1` is tolerated). */
  baseUrl?: string
  /** nyro admin token; secret — redacted on settings read-back. */
  adminToken?: string
  /** Rows with `enabled: false` stay configured but are not queried. */
  enabled?: boolean
}

/** The config fields gateway resolution reads (scalars or volatile refs). */
export interface InstanceSource {
  name?: unknown
  baseUrl?: unknown
  adminToken?: unknown
  instances?: unknown
}

/** One gateway the routes query. */
export interface ResolvedInstance {
  id: string
  name: string
  baseUrl: string
  adminToken: string
}

/** Which configured gateways hold a token (settings card read-back; never the token itself). */
export interface TokenFlags {
  /** Whether the primary gateway has a token. */
  primary: boolean
  /** Per `instances[]` index, including disabled and URL-less rows. */
  rows: boolean[]
}

/** Read a possibly-volatile string field, trimmed. */
function str(value: unknown): string {
  const plain = derefVolatile<unknown>(value, '')
  return typeof plain === 'string' ? plain.trim() : ''
}

/** The `instances[]` rows as plain records (non-object rows become `{}`, keeping indices aligned). */
function rowsOf(source: InstanceSource): Record<string, unknown>[] {
  const list = derefVolatile<unknown>(source.instances, [])
  if (!Array.isArray(list)) return []
  return list.map((raw) => {
    const row = derefVolatile<unknown>(raw, undefined)
    return typeof row === 'object' && row !== null ? row as Record<string, unknown> : {}
  })
}

/** Display fallback: the base URL's host, or the URL itself. */
function hostLabel(baseUrl: string): string {
  try {
    return new URL(baseUrl).host || baseUrl
  } catch {
    return baseUrl
  }
}

/**
 * Slugify a display name into an id: lowercase, unicode letters/digits kept,
 * every other run collapsed to `-`. '' when nothing usable remains.
 * @param name - the display name.
 * @returns the slug.
 */
export function slugifyInstanceName(name: string): string {
  return name.trim().toLowerCase().replace(/[^\p{L}\p{N}]+/gu, '-').replace(/^-+|-+$/g, '')
}

/**
 * Resolve the configured gateways, primary first, then `instances[]` in
 * order. Rows without a base URL and rows with `enabled: false` are skipped;
 * a row without a token is kept so the panel can report it. Ids are made
 * unique with `-2`, `-3`… suffixes; `default` is reserved for the primary.
 * @param source - raw plugin config (scalars or volatile refs).
 * @returns the gateways the routes should query.
 */
export function resolveInstances(source: InstanceSource): ResolvedInstance[] {
  const result: ResolvedInstance[] = []
  const taken = new Set<string>([PRIMARY_INSTANCE_ID])
  const primaryUrl = normalizeBaseUrl(str(source.baseUrl))
  if (primaryUrl !== '') {
    result.push({
      id: PRIMARY_INSTANCE_ID,
      name: str(source.name) || hostLabel(primaryUrl),
      baseUrl: primaryUrl,
      adminToken: str(source.adminToken),
    })
  }
  rowsOf(source).forEach((row, index) => {
    if (derefVolatile<unknown>(row.enabled, true) === false) return
    const baseUrl = normalizeBaseUrl(str(row.baseUrl))
    if (baseUrl === '') return
    const name = str(row.name)
    const stem = slugifyInstanceName(name) || `nyro-${index + 1}`
    let id = stem
    for (let n = 2; taken.has(id); n++) id = `${stem}-${n}`
    taken.add(id)
    result.push({ id, name: name || hostLabel(baseUrl), baseUrl, adminToken: str(row.adminToken) })
  })
  return result
}

/**
 * Report which configured gateways hold a token, by config position.
 * @param source - raw plugin config (scalars or volatile refs).
 * @returns token presence for the primary and each `instances[]` row.
 */
export function tokenFlags(source: InstanceSource): TokenFlags {
  return {
    primary: str(source.adminToken) !== '',
    rows: rowsOf(source).map(row => str(row.adminToken) !== ''),
  }
}
