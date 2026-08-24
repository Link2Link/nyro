/**
 * Shared panel helpers: the active-dictionary pick (document-language
 * based, task-board / dsh-ssh precedent) bound to the plugin's
 * interpolator, plus a small error-message extractor.
 */
import { en, format, zh, type NyroUsageKey } from './locales.ts'

/** Template values accepted by the interpolator. */
export type TranslateValues = Record<string, string | number>

/** Active dictionary, picked by the document language at call time. */
export function dictionary(): Record<string, string> {
  const lang = typeof document !== 'undefined' ? document.documentElement.lang : 'zh'
  return lang.toLowerCase().startsWith('en') ? { ...en } : { ...zh }
}

/** Translate a key with optional {name} template params (current language). */
export function tt(key: NyroUsageKey, values?: TranslateValues): string {
  const text = dictionary()[key] ?? key
  return values === undefined ? text : format(text, values)
}

/** Human-readable error text from an unknown thrown value. */
export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message
  return String(error)
}

/** Compact relative age like "2h30m" / "3d12h" / "45s". */
export function agoLabel(timestamp: string | null | undefined): string {
  if (timestamp === null || timestamp === undefined) return ''
  const diffMs = Date.now() - new Date(timestamp).getTime()
  if (!Number.isFinite(diffMs)) return ''
  if (diffMs < 0) return '0s'
  const seconds = Math.floor(diffMs / 1000)
  if (seconds < 60) return `${seconds}s`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes}m`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}h${minutes % 60}m`
  const days = Math.floor(hours / 24)
  return `${days}d${hours % 24}h`
}

/** Compact countdown like "2h30m" / "3d12h"; empty when already past. */
export function countdownLabel(resetsAt: string | null | undefined): string {
  if (resetsAt === null || resetsAt === undefined) return ''
  const diffMs = new Date(resetsAt).getTime() - Date.now()
  if (!Number.isFinite(diffMs) || diffMs <= 0) return ''
  const seconds = Math.floor(diffMs / 1000)
  if (seconds < 60) return `${seconds}s`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes}m`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}h${minutes % 60}m`
  const days = Math.floor(hours / 24)
  return `${days}d${hours % 24}h`
}
