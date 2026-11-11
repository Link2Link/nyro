import type { ProviderAccountView } from "@/lib/devin-account-pool";
import { tierLabel } from "@/lib/provider-usage";
import { useUsageClock } from "@/lib/use-usage-clock";
import { UsageProgress } from "@/components/usage-progress";

/** Passive headers are partial evidence, never a replacement for full usage. */
export function ClaudeQuotaObservationView({ account, isZh, fullSnapshotObservedAt }: {
  account: ProviderAccountView; isZh: boolean; fullSnapshotObservedAt?: number | null;
}) {
  const now = useUsageClock();
  const windows = account.claude_quota_observation?.windows ?? [];
  if (!windows.length) return null;
  const fullObservedAt = [account.claude_usage ? account.observed_at : null, fullSnapshotObservedAt]
    .filter((time): time is number => time != null && time > 0 && time <= now + 60_000
      && Number.isFinite(new Date(time).getTime()))
    .reduce<number | null>((latest, time) => latest == null ? time : Math.max(latest, time), null);
  return <section className="space-y-2 rounded border border-slate-200 p-2 text-xs" data-claude-quota-observation>
    <h4 className="font-medium text-slate-600">{isZh ? "推理响应配额（部分观测）" : "Inference response quota (partial)"}</h4>
    <p className="text-[10px] text-slate-500">{isZh
      ? "仅含响应头报告的窗口；不替代完整用量快照，也不参与账号池汇总。状态不是用量百分比。"
      : "Only windows reported by response headers; does not replace full usage or contribute to pool totals. Status is not a usage percentage."}</p>
    {windows.map((window, index) => {
      const used = window.used_percent != null && Number.isFinite(window.used_percent)
        && window.used_percent >= 0 && window.used_percent <= 100 ? window.used_percent : null;
      const observedAt = window.observed_at > 0 && Number.isFinite(new Date(window.observed_at).getTime())
        && window.observed_at <= now + 60_000 ? window.observed_at : null;
      const stale = observedAt == null || now - observedAt > 660_000;
      const older = fullObservedAt != null && observedAt != null && observedAt < fullObservedAt;
      const newer = fullObservedAt != null && observedAt != null && observedAt > fullObservedAt;
      const reset = window.resets_at && Number.isFinite(Date.parse(window.resets_at)) ? window.resets_at : null;
      const expired = reset != null && Date.parse(reset) <= now;
      const label = tierLabel(window.name, isZh);
      return <div key={`${window.name}:${index}`} className="space-y-1 text-slate-500" data-claude-observation-window={window.name} data-older-than-full={older || undefined}>
        <p className="font-medium">{label}</p>
        {older && <p className="text-amber-700">{isZh ? "早于完整用量快照；以较新的完整快照为准。" : "Older than the full usage snapshot; prefer the newer full snapshot."}</p>}
        {newer && <p>{isZh ? "晚于完整用量快照；完整快照时间保持不变。" : "Newer than the full usage snapshot; full snapshot time is unchanged."}</p>}
        {stale && <p className="text-amber-700">{isZh ? "旧数据 / 观测时间未知" : "Stale / observation time unknown"}</p>}
        {expired && <p className="text-amber-700">{isZh ? "窗口已过期；等待新观测，不推断重置后的用量。" : "Expired window; awaiting a new observation, not inferred reset usage."}</p>}
        {used != null ? <>
          <p className="tabular-nums">{isZh ? "已用" : "Used"} {used.toFixed(1)}%</p>
          {!older && !stale && !expired && <UsageProgress usedPercent={used} pacePercent={null} label={label + (isZh ? " 部分观测已用额度" : " partial observed quota")} isZh={isZh} />}
        </> : <p>{isZh ? "用量百分比未知" : "Usage percentage unknown"}</p>}
        <p>{isZh ? "状态" : "Status"}: {window.status?.trim() || (isZh ? "未知" : "Unknown")}</p>
        <p className="text-[10px]">{isZh ? "来源：推理响应头" : "Source: inference response headers"} <code>{window.source}</code></p>
        <p className="text-[10px]">{isZh ? "观测时间" : "Observed at"}: {observedAt != null
          ? <time dateTime={new Date(observedAt).toISOString()}>{new Date(observedAt).toLocaleString()}</time>
          : (isZh ? "时间未知" : "time unknown")}</p>
        <p className="text-[10px]">{reset
          ? <>{isZh ? "重置" : "Resets"}: <time dateTime={reset}>{new Date(reset).toLocaleString()}</time></>
          : (isZh ? "重置时间未知" : "Reset time unknown")}</p>
      </div>;
    })}
  </section>;
}
