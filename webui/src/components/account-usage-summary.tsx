import type { AccountUsagePeriod } from "@/lib/account-usage-stats";
import { formatTokenCount } from "@/lib/format";
import type { Locale } from "@/lib/i18n";
import { equivalentCostDisplay, formatUsageCost } from "@/lib/token-usage";
import type { AccountUsageStats, AccountUsageTotals } from "@/lib/types";

function accountPrice(totals: AccountUsageTotals, isZh: boolean, locale: Locale) {
  if (totals.request_count === 0) {
    return { text: formatUsageCost(0, "USD", locale), title: isZh ? "保留日志中无请求" : "No requests in retained logs" };
  }
  // Keep native currencies separate: the shared formatter otherwise converts mixed totals to USD.
  const prices = totals.equivalent_costs.map((cost) => ({
    currency: cost.currency,
    display: equivalentCostDisplay([cost], totals.unpriced_request_count, isZh, locale),
  })).filter(({ display }) => display.priced);
  if (!prices.length) return equivalentCostDisplay([], totals.unpriced_request_count, isZh, locale);
  return {
    text: prices.map(({ display }) => display.text.replace(/ \*$/, "")).join(" + ") + (totals.unpriced_request_count > 0 ? " *" : ""),
    title: prices.map(({ currency, display }) => `${currency}: ${display.title}`).join("\n"),
  };
}

/** Log-based metrics only; upstream quota, account actions and permissions stay separate. */
export function AccountUsageSummary({ stats, period, periodStart, loading, error, locale }: {
  stats?: AccountUsageStats;
  period: AccountUsagePeriod;
  periodStart: number;
  loading: boolean;
  error: boolean;
  locale: Locale;
}) {
  const isZh = locale === "zh-CN";
  const scope = (isZh
    ? "按稳定账号 ID 累计保留日志（含失败与重试），清理日志会减少累计值；仅包含已记录且带账号标识的用量，无法还原旧日志缺失的账号标识或未上报的 Token。非上游额度或永久账单。等价 API 价格按当前参考价估算。"
    : "Retained logs for this account ID, including failed and retried attempts; clearing logs reduces these totals. Only recorded usage with an account ID is included; missing account IDs in older logs or unreported tokens cannot be recovered. This is not upstream quota or a lifetime bill. Equivalent API price is estimated at current reference rates.") + (period === "month"
      ? (isZh ? " Kiro Token 为估算值，不代表实际 Credit 用量。" : " Kiro tokens are estimates, not actual credit usage.") : "");
  const status = error
    ? stats ? (isZh ? "刷新失败 · 当前为旧数据" : "Refresh failed · showing stale totals") : (isZh ? "用量统计读取失败" : "Usage totals unavailable")
    : !stats ? loading ? (isZh ? "正在读取统计…" : "Loading totals…") : (isZh ? "暂无账号统计" : "Account totals unavailable") : "";
  const periodTitle = isZh
    ? `浏览器本地时区，自 ${new Date(periodStart).toLocaleString(locale)} 至今；${period === "week" ? "自然周从周一开始" : "自然月从 1 号开始"}，非上游额度周期。`
    : `Browser local time, since ${new Date(periodStart).toLocaleString(locale)}; ${period === "week" ? "calendar week starts Monday" : "calendar month starts on the 1st"}, not the upstream quota window.`;
  const rows = [
    { key: "all_time" as const, label: isZh ? "全部记录" : "All records", title: scope },
    { key: "period" as const, label: period === "week" ? (isZh ? "本周" : "This week") : (isZh ? "本月" : "This month"), title: `${scope}\n${periodTitle}` },
  ];
  return <div data-account-usage={stats?.account_id ?? "unavailable"} aria-busy={loading && !stats}
    className="mt-2 w-full min-w-60 max-w-md rounded-lg border border-slate-200/80 bg-white/70 px-2.5 py-2" title={scope}>
    <div className="grid grid-cols-[4.5rem_minmax(0,1fr)_minmax(0,1.4fr)] items-baseline gap-x-2 gap-y-1.5">
      <span aria-hidden="true" />
      <span className="text-[10px] text-slate-500">{isZh ? "总 Token" : "Total tokens"}</span>
      <span className="text-[10px] text-slate-500">{isZh ? "等价 API 价格" : "Equivalent API price"}</span>
      {rows.map(({ key, label, title }) => {
        const totals = stats?.[key];
        const price = totals ? accountPrice(totals, isZh, locale) : null;
        return <div key={key} className="contents">
          <span className="text-[10px] font-medium text-slate-500" title={title}>{label}</span>
          <span className="text-xs font-semibold tabular-nums text-slate-700" title={`${title}\n${totals ? totals.total_tokens.toLocaleString(locale) : status}`}>
            {totals ? formatTokenCount(totals.total_tokens) : "—"}
          </span>
          <span className={`break-words text-xs font-semibold tabular-nums ${totals?.unpriced_request_count ? "text-amber-700" : "text-slate-700"}`} title={`${title}\n${price?.title ?? status}`}>
            {price?.text ?? "—"}
          </span>
        </div>;
      })}
    </div>
    {status && <p role="status" className={`mt-1.5 text-[10px] ${error ? "text-amber-700" : "text-slate-400"}`}>{status}</p>}
  </div>;
}
