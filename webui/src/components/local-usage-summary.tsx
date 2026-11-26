import { formatTokenCount } from "@/lib/format";
import type { Locale } from "@/lib/i18n";
import { retainedUsagePriceDisplay } from "@/lib/token-usage";
import type { LocalUsageKind } from "@/lib/provider-usage";
import type { ProviderApiPriceStats } from "@/lib/types";

const BILLING: Record<LocalUsageKind, { en: string; zh: string }> = {
  sub2api: { en: "Sub2API billing", zh: "Sub2API 实际账单" },
  // The default channel may point at a relay, so name neither vendor nor relay.
  anthropic_api: { en: "upstream billing", zh: "上游实际账单" },
};

/** Retained-log totals for providers without an upstream quota observation. */
export function LocalUsageSummary({ kind, stats, now, loading, locale = "en-US" }: {
  kind: LocalUsageKind;
  stats?: ProviderApiPriceStats | null;
  now: number;
  loading: boolean;
  locale?: Locale;
}) {
  const zh = locale === "zh-CN";
  const scope = zh
    ? `仅统计本供应商在 Nyro 保留的上游尝试（含失败和重试）。Token 为日志已记录用量，API 价格按当前参考价估算，非${BILLING[kind].zh}。`
    : `Retained Nyro logs for this provider, including failed and retried upstream attempts. Tokens are recorded usage; API prices use current reference rates, not ${BILLING[kind].en}.`;
  const start = stats?.period_start;
  const end = stats?.period_end;
  const month = stats?.period_tier === "calendar_month" && start != null && end != null
    && Number.isFinite(start) && Number.isFinite(end) && start <= now && now < end ? stats.period : null;
  const scopes = [
    { key: "all_time", label: zh ? "累计" : "All time", hint: scope, totals: stats?.all_time },
    { key: "calendar_month", label: zh ? "本月" : "This month", totals: month,
      hint: zh ? "按 UTC 自然月统计，不是上游额度重置周期。" : "UTC calendar month, not an upstream quota reset cycle." },
  ];

  return <div data-local-usage={kind} aria-busy={loading && !stats} className="border-t border-slate-200/70 pt-2" title={scope}>
    <div className="grid grid-cols-2 gap-x-4 sm:gap-x-6">
      {scopes.map(({ key, label, hint, totals }) => {
        const price = totals ? retainedUsagePriceDisplay(totals, zh, locale) : null;
        const fields = [
          { key: "attempts", label: zh ? "上游请求" : "Upstream attempts", text: totals?.request_count.toLocaleString(locale) },
          { key: "tokens", label: zh ? "已记录 Token" : "Recorded tokens", text: totals ? formatTokenCount(totals.total_tokens) : undefined,
            title: totals?.total_tokens.toLocaleString(locale) },
          { key: "price", label: zh ? "等价 API 价格" : "Equivalent API price", text: price?.text, title: price?.title,
            partial: Boolean(totals?.unpriced_request_count) },
        ];
        return <div key={key} data-local-usage-scope={key} className="min-w-0">
          <p className="mb-1 text-xs font-medium text-slate-700" title={hint}>{label}</p>
          <dl className="space-y-1">
            {fields.map((field) => <div key={field.key} className="flex min-w-0 flex-wrap items-baseline justify-between gap-x-2">
              <dt className="text-[10px] font-medium text-slate-500">{field.label}</dt>
              <dd className={`min-w-0 break-words text-xs font-semibold tabular-nums ${field.partial ? "text-amber-700" : "text-slate-700"}`}
                title={field.title}>{field.text ?? "—"}</dd>
            </div>)}
          </dl>
        </div>;
      })}
    </div>
    {!stats && <p role="status" className="mt-1.5 text-[10px] text-slate-400">
      {loading ? (zh ? "正在读取用量统计…" : "Loading usage totals…") : (zh ? "本地用量统计暂不可用" : "Local usage totals unavailable")}
    </p>}
    {stats && !month && <p role="status" className="mt-1.5 text-[10px] text-amber-700">
      {zh ? "本月统计尚未更新" : "Current-month totals not yet available"}
    </p>}
  </div>;
}
