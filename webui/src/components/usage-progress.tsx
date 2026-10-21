import { utilizationBarClass } from "@/lib/usage-pace";

/** Shared quota track and steady-pace needle for providers, pools and accounts. */
export function UsageProgress({ usedPercent, pacePercent = null, label, isZh, weighted = false, weightBasis, className = "" }: {
  usedPercent: number;
  pacePercent?: number | null;
  label: string;
  isZh: boolean;
  weighted?: boolean;
  /** Pool-specific weighting description; defaults to Devin's Pro-equivalent capacity. */
  weightBasis?: string;
  className?: string;
}) {
  if (!Number.isFinite(usedPercent)) return null;
  const used = Math.min(100, Math.max(0, usedPercent));
  const pace = pacePercent != null && Number.isFinite(pacePercent)
    ? Math.min(100, Math.max(0, pacePercent)) : null;
  const basis = weightBasis ?? (isZh ? "各账号窗口进度与 Pro 等价容量" : "each account's window progress and Pro-equivalent capacity");
  const paceTitle = pace == null ? undefined : weighted
    ? (isZh
      ? `匀速参考线：${pace.toFixed(1)}%（按${basis}加权，仅供参考）`
      : `Steady pace: ${pace.toFixed(1)}% (weighted by ${basis}; estimate only)`)
    : (isZh
      ? `匀速参考线：${Math.round(pace)}%（按整窗匀速使用推进）`
      : `Steady pace: ${Math.round(pace)}% (even consumption over the window)`);
  return <div role="progressbar" aria-label={label} aria-valuemin={0} aria-valuemax={100} aria-valuenow={used}
    className={`relative h-2 min-w-20 flex-1 overflow-hidden rounded-full bg-slate-100 ${className}`}>
    <div className={`h-full rounded-full transition-all ${utilizationBarClass(used)}`} style={{ width: `${used}%` }} />
    {pace != null && <div
      data-steady-pace-marker={weighted ? "weighted" : "account"}
      className="absolute inset-y-0 w-[3px] -translate-x-1/2 rounded-full bg-slate-700 shadow-[0_0_0_1.5px_rgba(255,255,255,0.9)]"
      style={{ left: `${pace}%` }} title={paceTitle} />}
  </div>;
}
