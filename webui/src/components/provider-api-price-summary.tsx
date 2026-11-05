import type { Locale } from "@/lib/i18n";
import { tierLabel } from "@/lib/provider-usage";
import { retainedUsagePriceDisplay } from "@/lib/token-usage";
import type { ProviderUsage } from "@/lib/types";

/** Display backend-priced retained attempts; never infer cycles or change routing quota. */
export function ProviderApiPriceSummary({ usage, now, stale, loading, locale = "en-US", bucketLabel, scopeKey, totalOnly = false, projectionAllowed = true }: {
  usage: Pick<ProviderUsage, "provider_id" | "tiers" | "api_price">;
  now: number;
  stale: boolean;
  loading: boolean;
  locale?: Locale;
  /** Scope labels describe backend attribution only; no model matching happens here. */
  bucketLabel?: string;
  scopeKey?: string;
  /** Independent bucket cycles must never be aggregated on the provider footer. */
  totalOnly?: boolean;
  /** Unresolved/catalog bucket observations cannot support matched capacity. */
  projectionAllowed?: boolean;
}) {
  const isZh = locale === "zh-CN";
  const stats = usage.api_price;
  const scope = bucketLabel
    ? (isZh
      ? `仅统计主要归属为“${bucketLabel}”的保留日志（含失败与重试），不重复累计其窗口或共享约束。清理日志会减少累计值。等价 API 价格按已记录 Token 和当前参考价估算，非实际账单或套餐余额。`
      : `Retained logs primarily attributed to “${bucketLabel}”, including failed and retried attempts; windows and shared constraints are not counted again. Clearing logs reduces these totals. Equivalent API price uses recorded tokens and current reference rates, not a bill or plan balance.`)
    : isZh
      ? "按此供应商 ID 累计保留日志（含失败与重试），清理日志会减少累计值；不包含网关外的使用。等价 API 价格按已记录 Token 和当前参考价估算，非上游实际账单或套餐余额。"
      : "Retained logs for this provider ID, including failed and retried attempts; clearing logs reduces these totals. Usage outside this gateway is not included. Equivalent API price uses recorded tokens and current reference rates, not an upstream bill or plan balance.";
  const start = stats?.period_start;
  const end = stats?.period_end;
  const hasBounds = start != null && end != null && 0 <= start && start < end
    && Number.isFinite(new Date(start).getTime()) && Number.isFinite(new Date(end).getTime());
  // Validate the server interval only: an expired snapshot never rolls into a new cycle.
  const period = hasBounds && start <= now && now < end ? stats?.period : null;
  const cycleStatus = !stats || period ? "" : hasBounds && now >= end
    ? (isZh ? "本周期已结束，请刷新用量。" : "Current cycle ended; refresh usage.")
    : (isZh ? "本周期未知，请刷新用量。" : "Current cycle unknown; refresh usage.");
  const cycleScope = isZh
    ? "由后端按上游报告的重置周期确定，而非自然周或自然月。优先使用每周额度；未报告周额度时，GLM / MiniMax / OpenAI Codex 使用已识别的 5 小时周期，xAI 使用月度周期。不合并不同额度窗口；OpenAI Codex 的功能专用窗口和时长未知的窗口不用于本周期统计。Google OAuth 订阅（Antigravity / Gemini CLI）仅使用官方已识别的每周窗口，无周窗口时使用已识别的 5 小时窗口；不从模型目录的 Gemini 共享池或时长未知的主要/次要窗口推断周期。"
    : "Determined by the backend from the upstream-reported reset cycle, not a calendar week or month. Weekly quota is preferred; when no weekly tier is reported, GLM / MiniMax / OpenAI Codex use an identified 5h cycle and xAI uses the monthly cycle. Different quota windows are not combined; OpenAI Codex feature-specific and unknown-duration windows are not used for current-cycle statistics. Google OAuth subscriptions (Antigravity / Gemini CLI) use only identified official weekly windows, falling back to an identified 5h window when no weekly window is reported; cycles are not inferred from the catalog Gemini shared pool or unknown-duration primary/secondary windows.";
  const periodTitle = [scope, cycleScope, stats?.period_tier ? `${isZh ? "额度窗口" : "Quota window"}: ${tierLabel(stats.period_tier, isZh)}` : "", hasBounds
    ? (isZh
      ? `浏览器本地时间：${new Date(start).toLocaleString(locale)} → ${new Date(end).toLocaleString(locale)}（含开始，不含结束）。`
      : `Browser local time: ${new Date(start).toLocaleString(locale)} → ${new Date(end).toLocaleString(locale)} (start inclusive, end exclusive).`)
    : "", cycleStatus].filter(Boolean).join("\n");
  const usedPercent = stats?.used_percent;
  const tiers = usage.tiers.filter((tier) => tier.name === stats?.period_tier);
  // Prices and ratios arrive in one backend snapshot. Do not project a stale or mismatched one.
  const matchingTier = tiers.length === 1 && tiers[0].resets_at != null && Date.parse(tiers[0].resets_at) === end
    && tiers[0].used_percent === usedPercent;
  const costs = stats?.estimated_capacity;
  const canEstimate = projectionAllowed && !stale && period && period.request_count > 0 && matchingTier
    && period.equivalent_costs.some((cost) => Number.isFinite(cost.total_cost) && cost.total_cost >= 0 && cost.priced_request_count > 0)
    && usedPercent != null && Number.isFinite(usedPercent) && usedPercent > 0 && usedPercent <= 100
    && costs?.length && costs.every((cost) => Number.isFinite(cost.total_cost) && cost.total_cost >= 0 && cost.priced_request_count > 0);
  const estimatePrice = canEstimate ? retainedUsagePriceDisplay({ ...period, equivalent_costs: costs }, isZh, locale) : null;
  const estimateTitle = [periodTitle, isZh
    ? "本周期等价 API 价格 ÷ 同周期已用比例，线性外推用满 100% 时的总等价 API 价格（包含已用部分，并非剩余余额）。仅供参考，受日志覆盖、网关外的使用、模型组合和用量观察时间影响。"
    : "Current-cycle equivalent API price ÷ matching-cycle used fraction, linearly projected to 100% usage (includes usage so far, not remaining balance). Reference only; affected by log coverage, usage outside this gateway, model mix and usage observation time.",
  estimatePrice?.priced ? (isZh
    ? `用于估算的已用比例：${usedPercent!.toLocaleString(locale, { maximumFractionDigits: 4 })}%。`
    : `Used for projection: ${usedPercent!.toLocaleString(locale, { maximumFractionDigits: 4 })}%.`) : (isZh
    ? "暂无可用估算：需要有效的本周期价格及同周期用量比例（大于 0 且不超过 100%）；旧数据、周期不匹配或无参考价时不估算。"
    : "Estimate unavailable: requires a current-cycle price and matching-cycle usage above 0% and at most 100%; stale data, mismatched cycles or missing reference prices are not projected."),
  estimatePrice?.title ?? ""].filter(Boolean).join("\n");
  const rows = [
    { key: "all_time", label: totalOnly ? (isZh ? "供应商保留日志总价格" : "Provider retained-log total") : (isZh ? "累计 API 价格" : "Total API price"), title: scope, totals: stats?.all_time },
    ...(!totalOnly ? [{ key: "period", label: isZh ? "本周期 API 价格" : "Current-cycle API price", title: periodTitle, totals: period }] : []),
  ];
  return <div data-provider-api-price={usage.provider_id} data-price-scope={scopeKey} aria-busy={loading && !stats}
    className="border-t border-slate-200/70 pt-2" title={scope}>
    <dl className={`grid gap-x-6 gap-y-2 ${totalOnly ? "" : "sm:grid-cols-3"}`}>
      {rows.map(({ key, label, title, totals }) => {
        const price = totals ? retainedUsagePriceDisplay(totals, isZh, locale) : null;
        return <div key={key} data-api-price={key} className="min-w-0" title={title}>
          <dt className="text-[10px] font-medium text-slate-500">{label}</dt>
          <dd className={`mt-0.5 break-words text-xs font-semibold tabular-nums ${totals?.unpriced_request_count ? "text-amber-700" : "text-slate-700"}`}
            title={[title, price?.title].filter(Boolean).join("\n")}>
            {price?.text ?? "—"}
          </dd>
        </div>;
      })}
      {!totalOnly && <div data-api-price="estimated_capacity" className="min-w-0" title={estimateTitle}>
        <dt className="text-[10px] font-medium text-slate-500">{isZh ? "本周期预估可用 API 价格" : "Estimated cycle API capacity"}</dt>
        <dd className={`mt-0.5 break-words text-xs font-semibold tabular-nums ${period?.unpriced_request_count ? "text-amber-700" : "text-slate-700"}`}>
          {estimatePrice?.priced ? `≈ ${estimatePrice.text}${stats?.estimated_capacity_partial ? (isZh ? "（部分估算）" : " (partial)") : ""}` : "—"}
        </dd>
      </div>}
    </dl>
    {totalOnly && <p className="mt-1.5 text-[10px] text-slate-400">{isZh ? "含未归属用量；不合并独立桶的周期或容量。" : "Includes unassigned usage; independent bucket cycles and capacities are not combined."}</p>}
    {!totalOnly && cycleStatus && <p role="status" className="mt-1.5 text-[10px] text-amber-700">{cycleStatus}</p>}
    {!stats && <p role="status" className="mt-1.5 text-[10px] text-slate-400">
      {loading ? (isZh ? "正在读取金额统计…" : "Loading price totals…") : (isZh ? "API 金额统计暂不可用" : "API price totals unavailable")}
    </p>}
    {!totalOnly && stats && stale && <p role="status" className="mt-1.5 text-[10px] text-amber-700">
      {isZh ? "当前为旧数据，暂不估算可用金额，请刷新用量。" : "Showing stale totals; refresh usage to estimate capacity."}
    </p>}
  </div>;
}
