/**
 * One quota window row: tier label, utilization progress bar
 * (green < 70%, orange >= 70%, red >= 90%), used percentage, and a reset
 * countdown. Styling mirrors the nyro webui's coding-plan footer.
 */
import type { NyroTier } from '../../nyro.ts'
import { countdownLabel, tt } from '../tt.ts'
import css from './panel.module.css'

/** Pretty-print a tier name: known windows localized, the collapsed Google
 * Gemini bucket labeled explicitly, `feature:<f>:<w>` rendered as
 * "<Feature> · <window>", anything else humanized. */
export function tierLabel(name: string): string {
  const known = ['five_hour', 'weekly_limit', 'monthly', 'primary_window', 'secondary_window'] as const
  type TierKey = `tier.${(typeof known)[number]}`
  // Google folds every first-party Gemini family into one shared-bucket row
  // named `gemini`; label it as the pool it stands for.
  if (name === 'gemini') return tt('tier.gemini')
  const feature = /^feature:(.+):(five_hour|weekly_limit|monthly|primary_window|secondary_window)$/.exec(name)
  if (feature !== null) {
    const featureName = feature[1].replace(/[_-]+/g, ' ').replace(/\s+/g, ' ').trim()
      .replace(/\b\w/g, character => character.toUpperCase())
    return tt('tier.feature', { feature: featureName, window: tt(`tier.${feature[2]}` as TierKey) })
  }
  if ((known as readonly string[]).includes(name)) return tt(`tier.${name}` as TierKey)
  return name.replace(/[_-]+/g, ' ').replace(/\b\w/g, character => character.toUpperCase())
}

/** Utilization → progress-bar color state. */
function tierState(used: number): string {
  if (used >= 90) return 'danger'
  if (used >= 70) return 'warn'
  return 'ok'
}

/** Window length per tier name, used to place the steady-pace marker
 * (mirrors the nyro webui coding-plan footer). */
export function tierWindowMs(name: string): number | null {
  // Feature-specific Codex limits are displayed but are not equivalent to the
  // provider's main routing quota. Do not show a misleading steady-pace
  // marker for them even when the suffix names a known duration.
  if (name.startsWith('feature:')) return null
  switch (name) {
    case 'five_hour':
      return 5 * 3_600_000
    case 'weekly_limit':
      return 7 * 24 * 3_600_000
    case 'monthly':
      return 30 * 24 * 3_600_000
    default:
      return null
  }
}

/**
 * Steady-pace position (0-100): where a perfectly even consumer would sit
 * right now. Computed from the reset time walking the window backwards:
 * `elapsed / window` — 0 right after reset, 100 at reset time. Null when the
 * tier carries no reset time or an unknown window.
 */
export function steadyPacePercent(resetsAt: string | null | undefined, name: string, now: number): number | null {
  if (resetsAt === null || resetsAt === undefined) return null
  const windowMs = tierWindowMs(name)
  if (windowMs === null) return null
  const remaining = new Date(resetsAt).getTime() - now
  if (!Number.isFinite(remaining)) return null
  if (remaining <= 0) return 100
  if (remaining >= windowMs) return 0
  return ((windowMs - remaining) / windowMs) * 100
}

/** Props the tier row needs. */
export interface TierBarProps {
  tier: NyroTier
  /** Current time, tracked so the steady-pace marker re-evaluates on the 30s tick. */
  now: number
}

/** Render one quota-window row. */
export function TierBar(props: TierBarProps) {
  const used = Math.min(Math.max(Number.isFinite(props.tier.used_percent) ? props.tier.used_percent : 0, 0), 100)
  const countdown = countdownLabel(props.tier.resets_at)
  const state = tierState(used)
  const pace = steadyPacePercent(props.tier.resets_at, props.tier.name, props.now)
  return (
    <div className={css.tierRow} title={props.tier.name}>
      <span className={css.tierLabel}>{tierLabel(props.tier.name)}</span>
      <div className={css.tierTrack}>
        {pace !== null
          ? (
            // Steady-pace triangle above the track, pointing down at the
            // position an even consumer would be at right now (0% right
            // after reset → 100% at reset time). The wrapper sits in the
            // track's unclipped overflow area.
            <div className={css.tierPaceWrap}>
              <div
                className={css.tierPace}
                style={{ left: `${Math.min(Math.max(pace, 0), 100)}%` }}
                title={tt('tier.pace', { percent: Math.round(pace) })}
              />
            </div>
          )
          : null}
        <div
          className={`${css.tierFill} ${state === 'ok' ? css.tierFillOk : state === 'warn' ? css.tierFillWarn : css.tierFillDanger}`}
          style={{ width: `${used}%` }}
        />
      </div>
      <span className={`${css.tierPercent} ${state === 'ok' ? css.tierPercentOk : state === 'warn' ? css.tierPercentWarn : css.tierPercentDanger}`}>
        {Math.round(used)}%
      </span>
      <span className={css.tierReset} title={props.tier.resets_at ?? undefined}>
        {countdown === '' ? '' : `⏳ ${countdown}`}
      </span>
    </div>
  )
}
