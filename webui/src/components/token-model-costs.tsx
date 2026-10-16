import { useMemo, useState } from "react";
import { ChevronRight, Pencil } from "lucide-react";
import { formatTokenCount } from "@/lib/format";
import { useLocale } from "@/lib/i18n";
import { formatUsageCost, groupUsageModelFamilies, sumUsageCost, type UsageCostTotals } from "@/lib/token-usage";
import type { TokenUsageDay } from "@/lib/types";
import type { TokenPriceTarget } from "@/components/token-price-dialog";
import { Button } from "@/components/ui/button";

const PREVIEW_COUNT = 5;

/** Collapse display-only families; editing always targets the original provider/model. */
export function TokenModelCosts({ rows, currency, selectedDay, onEdit }: {
  rows: TokenUsageDay[];
  currency: string;
  selectedDay: string | null;
  onEdit: (target: TokenPriceTarget) => void;
}) {
  const { locale } = useLocale();
  const zh = locale === "zh-CN";
  const [showAll, setShowAll] = useState(false);
  const families = useMemo(() => groupUsageModelFamilies(rows, currency, "cost"), [rows, currency]);
  const total = sumUsageCost(rows, currency);
  const visible = showAll ? families : families.slice(0, PREVIEW_COUNT);
  const money = (cost: UsageCostTotals, field: "total_cost" | "input_cost" | "cache_read_cost" | "output_cost" = "total_cost") => cost.priced_request_count ? formatUsageCost(cost[field], currency, locale) : "—";
  const number = (value: number) => value.toLocaleString(locale);
  const incomplete = (cost: UsageCostTotals, missing: number) => missing > 0 || cost.unpriced_request_count + cost.other_currency_request_count > 0;
  const unitRate = (value: number, nativeCurrency: string) => new Intl.NumberFormat(locale, { style: "currency", currency: nativeCurrency, maximumFractionDigits: 8 }).format(value);

  return <div className="token-models token-family-list border-t pt-5">
    <div className="mb-3 flex flex-wrap items-baseline justify-between gap-2">
      <h3 className="text-sm font-semibold text-slate-800">{selectedDay ? `${selectedDay} · ` : ""}{zh ? "模型费用" : "Cost by model"}</h3>
      <span className="text-xs text-slate-500">{zh ? `${families.length} 个模型 · ${currency}` : `${families.length} models · ${currency}`}</span>
    </div>
    <p className="mb-3 text-xs text-slate-500">{zh ? "同模型变体与提供商合并 · 展开查看明细和设置价格" : "Variants and providers combined · Expand for details and prices"}</p>
    {families.length === 0 ? <p className="py-8 text-center text-sm text-slate-500">{zh ? "暂无已记录的模型消耗" : "No recorded model usage"}</p> : <>
      <div className="token-family-columns mb-1 px-2 text-[11px] text-slate-500" aria-hidden="true"><span>{zh ? "模型" : "Model"}</span><span className="text-right">{zh ? "合计费用" : "Total value"}</span><span className="hidden text-right sm:block">{zh ? "占比" : "Share"}</span></div>
      {visible.map((family) => {
        const share = total.total_cost > 0 && family.cost.priced_request_count ? `${(family.cost.total_cost / total.total_cost * 100).toFixed(1)}%` : "—";
        const partial = incomplete(family.cost, family.missing_usage_count);
        return <details key={family.key} className="token-family border-b" data-family={family.model}>
          <summary className="token-family-columns cursor-pointer rounded-lg px-2 py-3 focus-visible:outline-2 focus-visible:outline-offset-2" aria-label={`${zh ? "展开模型费用" : "Expand model costs"}: ${family.model || (zh ? "未知模型" : "Unknown model")}`}>
            <span className="flex min-w-0 items-center gap-2"><ChevronRight className="token-family-chevron h-3.5 w-3.5 shrink-0 text-slate-400" /><span className="min-w-0"><span className="block break-words text-xs font-medium text-slate-800">{family.model || (zh ? "未知模型" : "Unknown model")}</span><span className="mt-1 block text-[10px] text-slate-500">{formatTokenCount(family.total_tokens)} tokens</span></span></span>
            <span className="whitespace-nowrap text-right text-sm font-semibold tabular-nums text-slate-900" title={partial ? (zh ? "部分用量或价格缺失，仅含已知费用" : "Partial usage or prices; known value only") : undefined}>{money(family.cost)}{partial && <span className="ml-1 text-amber-600" aria-label={zh ? "部分数据缺失" : "Partial data"}>*</span>}</span>
            <span className="hidden text-right text-xs tabular-nums text-slate-500 sm:block">{share}</span>
          </summary>
          <div className="token-family-detail mb-3 rounded-lg border p-3">
            <div className="mb-3 flex flex-wrap justify-between gap-2 text-[11px] text-slate-500"><span>{zh ? `${family.variant_count} 个变体 · ${family.provider_count} 个提供商 · ${number(family.request_count)} 次尝试` : `${family.variant_count} variants · ${family.provider_count} providers · ${number(family.request_count)} attempts`}</span><span>{zh ? "逐项计价后合并，不改变原始单价" : "Summed after individual pricing; original rates unchanged"}</span></div>
            <dl className="mb-3 grid grid-cols-3 gap-2 text-[11px]">{([
              [zh ? "普通输入" : "Input", "input_cost"], [zh ? "缓存读取" : "Cache", "cache_read_cost"], [zh ? "输出" : "Output", "output_cost"],
            ] as const).map(([label, field]) => <div key={field}><dt className="text-slate-500">{label}</dt><dd className="mt-1 break-words font-medium text-slate-800 tabular-nums">{money(family.cost, field)}</dd></div>)}</dl>
            {partial && <p className="mb-3 text-[11px] text-amber-600">{zh ? `仅汇总已记录、已计价部分；${number(family.cost.unpriced_request_count)} 次尝试未计价，${number(family.missing_usage_count)} 次用量不完整。` : `Recorded, priced portions only; ${number(family.cost.unpriced_request_count)} unpriced attempts, ${number(family.missing_usage_count)} with incomplete usage.`}</p>}
            {family.variants.map((variant) => <div key={variant.key} className="token-family-variant border-t py-3 last:pb-0" data-provider={variant.provider_id} data-model={variant.model}>
              <div className="flex items-start justify-between gap-2"><div className="min-w-0"><p className="break-words text-xs font-medium text-slate-800">{variant.model || (zh ? "未知模型" : "Unknown model")}</p><p className="mt-1 break-words text-[11px] text-slate-500">{variant.provider_name || (zh ? "未知提供商" : "Unknown provider")} · {formatTokenCount(variant.total_tokens)} tokens</p></div><span className="shrink-0 text-xs font-medium tabular-nums text-slate-800">{money(variant.cost)}{incomplete(variant.cost, variant.missing_usage_count) && " *"}</span></div>
              <div className="mt-2 flex flex-wrap items-start justify-between gap-2"><div className="min-w-0 flex-1 text-[10px] leading-relaxed text-slate-500">{variant.pricing ? <>
                <p className="break-words">{variant.is_manual ? (zh ? "自定义价格" : "Custom prices") : (zh ? "参考价格" : "Reference rates")} · {variant.pricing.pricing_model}</p>
                <p className="break-words">{variant.pricing.currency} / 1M · {zh ? "输入 / 缓存 / 输出" : "Input / cache / output"}: {[variant.pricing.input_rate, variant.pricing.cache_read_rate, variant.pricing.output_rate].map((value) => unitRate(value, variant.pricing!.currency)).join(" / ")}</p>
                {variant.cost.cache_rate_fallback_count > 0 && <p>{zh ? "缓存单价缺失，按输入价估算" : "Cache rate unavailable; uses input rate"}</p>}
              </> : <p>{zh ? "未计价，不表示免费" : "Unpriced, not free"}</p>}</div>
                {!!variant.provider_id.trim() && !!variant.model.trim() && <Button variant="outline" size="sm" className="h-7 shrink-0 px-2 text-[11px]" aria-label={`${zh ? "设置价格" : "Set prices"}: ${variant.model} · ${variant.provider_name}`} onClick={() => onEdit(variant)}><Pencil className="h-3 w-3" />{zh ? "设置价格" : "Set prices"}</Button>}
              </div>
            </div>)}
          </div>
        </details>;
      })}
      {families.length > PREVIEW_COUNT && <div className="mt-3 flex items-center justify-between gap-2 text-xs text-slate-500"><span>{zh ? `显示 ${visible.length} / ${families.length} 个模型` : `Showing ${visible.length} / ${families.length} models`}</span><Button variant="ghost" size="sm" onClick={() => setShowAll(!showAll)}>{showAll ? (zh ? "收起列表" : "Show less") : (zh ? `查看全部 ${families.length} 个模型` : `Show all ${families.length} models`)}</Button></div>}
    </>}
  </div>;
}
