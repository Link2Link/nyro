/** Claude pool summaries are weighted estimates, not exact scheduling windows. */
import type { NyroPoolQuotaWindow } from '../../nyro.ts'
import { tt } from '../tt.ts'
import { tierLabel } from './TierBar.tsx'
import css from './panel.module.css'

function percent(value: number): string {
  return String(Math.round(value * 10) / 10)
}

export function PoolQuota(props: {
  windows: { name: string; quota: NyroPoolQuotaWindow }[]
  estimated: boolean
  now: number
}) {
  return <div className={css.poolSummary}>
    <div className={css.poolHeading}>{tt(props.estimated ? 'pool.estimated' : 'pool.summary')}</div>
    {props.windows.map(({ name, quota }) => {
      // Unknown/stale coverage must never be presented as a full pool at 0%.
      const complete = quota.capacity_complete === true && quota.stale === false
        && Number.isInteger(quota.total_accounts) && quota.total_accounts > 0
        && quota.known_accounts === quota.total_accounts
      const used = complete && typeof quota.used_percent === 'number'
        && Number.isFinite(quota.used_percent) && quota.used_percent >= 0 && quota.used_percent <= 100
        ? quota.used_percent : null
      const remaining = used !== null && typeof quota.remaining_percent === 'number'
        && Number.isFinite(quota.remaining_percent) && quota.remaining_percent >= 0 && quota.remaining_percent <= 100
        ? quota.remaining_percent : null
      const state = used !== null && used >= 90 ? 'danger' : used !== null && used >= 70 ? 'warn' : 'ok'
      const reset = quota.next_reset_at == null ? NaN : Date.parse(quota.next_reset_at)
      const resetKnown = Number.isFinite(reset) && Number.isInteger(quota.next_reset_accounts)
        && quota.next_reset_accounts > 0 && quota.next_reset_accounts <= quota.total_accounts
      const count = (value: number) => Number.isInteger(value) && value >= 0 ? value : '?'
      return <div className={css.poolWindow} key={name}>
        <div className={css.poolWindowHeader}>
          <span>{tierLabel(name)}</span>
          <span>{used === null ? tt('pool.unknown') : tt('pool.used', { percent: percent(used) })}</span>
        </div>
        {used !== null ? <div className={css.tierTrack} role="progressbar" aria-label={tierLabel(name)} aria-valuemin={0} aria-valuemax={100} aria-valuenow={used}>
          <div className={`${css.tierFill} ${state === 'danger' ? css.tierFillDanger : state === 'warn' ? css.tierFillWarn : css.tierFillOk}`} style={{ width: `${used}%` }} />
        </div> : null}
        <div className={css.poolMeta}>
          {remaining !== null ? `${tt('pool.remaining', { percent: percent(remaining) })} · ` : ''}
          {tt('pool.coverage', { known: count(quota.known_accounts), total: count(quota.total_accounts) })}
          {quota.stale ? ` · ${tt('pool.stale')}` : ''}
        </div>
        {resetKnown ? <div className={css.poolMeta}>
          {tt(reset <= props.now ? 'pool.resetPassed' : 'pool.nextReset', { count: quota.next_reset_accounts })}
          {' '}<time dateTime={quota.next_reset_at!}>{new Date(reset).toLocaleString()}</time>
        </div> : null}
        {quota.reset_mode !== 'aligned' ? <div className={css.poolMeta}>
          {tt(quota.reset_mode === 'staggered' ? 'pool.staggered' : 'pool.resetUnknown')}
        </div> : null}
      </div>
    })}
    <div className={css.poolMeta}>{tt('pool.displayOnly')}</div>
  </div>
}
