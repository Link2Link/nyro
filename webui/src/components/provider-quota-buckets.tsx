import { AlertCircle, ArrowUpRight, Clock } from "lucide-react";
import type { Locale } from "@/lib/i18n";
import { tierLabel, providerUsagePause } from "@/lib/provider-usage";
import { formatTokenCount } from "@/lib/format";
import { retainedUsagePriceDisplay } from "@/lib/token-usage";
import { countdownStr, utilizationColor } from "@/lib/usage-pace";
import type { ProviderUsage, QuotaBucket, QuotaBucketFilter } from "@/lib/types";
import { UsageProgress } from "@/components/usage-progress";
import { ProviderApiPriceSummary } from "@/components/provider-api-price-summary";

type BucketState = "available" | "exhausted" | "unknown";
const identity = (bucket: QuotaBucket) => JSON.stringify([bucket.scope_id, bucket.id]);
const bucketStale = (bucket: QuotaBucket, now: number, refreshFailed: boolean) => {
  const observed = Date.parse(bucket.queried_at);
  return refreshFailed || bucket.stale || !Number.isFinite(observed) || now - observed >= 5 * 60_000;
};
// Summarize backend decisions, never percentages or guessed model memberships.
function bucketState(bucket: QuotaBucket, stale: boolean): BucketState {
  if (bucket.scheduling?.status === "quota_exhausted") return "exhausted";
  if (stale || bucket.binding.status !== "resolved" || !bucket.models.length || bucket.scheduling?.status !== "eligible"
    || bucket.source === "catalog" || ["unknown_mapping", "conflicting_binding"].includes(bucket.scheduling.reason ?? "")) return "unknown";
  return "available";
}
const stateClass = (state: string) => state === "available" ? "bg-emerald-50 text-emerald-700"
  : state === "exhausted" ? "bg-red-50 text-red-700" : "bg-amber-50 text-amber-700";
const stateLabel = (state: BucketState, zh: boolean) => state === "available" ? (zh ? "可调度" : "Available")
  : state === "exhausted" ? (zh ? "已暂停" : "Paused") : (zh ? "状态未知" : "Unknown");
const sourceLabel = (source: string, zh: boolean) => {
  const labels: Record<string, [string, string]> = {
    configured: ["Configured", "显式绑定"], official: ["Official", "官方绑定"],
    adapter: ["Validated adapter", "已验证适配规则"], google_exact_model_v1: ["Validated Google binding", "已验证 Google 绑定"],
    catalog_exact_model_v1: ["Catalog exact-model binding", "目录精确模型绑定"], enforced_summary: ["Enforced summary", "强制额度摘要"],
    model_catalog: ["Model catalog", "模型目录"], catalog: ["Model catalog", "模型目录"],
    unresolved: ["Unresolved", "未解析"], unknown: ["Unknown", "未知"],
  };
  return labels[source]?.[zh ? 1 : 0] ?? source;
};

export function QuotaBucketAvailability({ buckets, stale = false, isZh = false, now = Date.now() }: {
  buckets: QuotaBucket[]; stale?: boolean; isZh?: boolean; now?: number;
}) {
  const states = buckets.filter((bucket) => !["shared_constraint", "window_constraint"].includes(bucket.binding.status))
    .map((bucket) => bucketState(bucket, bucketStale(bucket, now, stale)));
  const available = states.filter((state) => state === "available").length;
  const exhausted = states.filter((state) => state === "exhausted").length;
  const state = states.length > 0 && available === states.length ? "available"
    : states.length > 0 && exhausted === states.length ? "exhausted"
      : available > 0 ? "partial" : "unknown";
  const label = state === "available" ? (isZh ? "全部可用" : "Available")
    : state === "partial" ? (isZh ? "部分可用" : "Partially available")
      : state === "exhausted" ? (isZh ? "全部暂停" : "Exhausted / unavailable") : (isZh ? "状态未知" : "Unknown");
  return <span data-quota-availability={state} className={`inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-[10px] font-semibold ${stateClass(state)}`}>
    <span aria-hidden="true" className="h-1.5 w-1.5 rounded-full bg-current" />
    {label}{available > 0 && ` · ${available}/${states.length}`}
  </span>;
}

function BucketCard({ usage, bucket, now, stale, loading, locale, onDetails }: {
  usage: ProviderUsage; bucket: QuotaBucket; now: number; stale: boolean; loading: boolean;
  locale: Locale; onDetails?: (filter: QuotaBucketFilter) => void;
}) {
  const zh = locale === "zh-CN";
  const isStale = bucketStale(bucket, now, stale);
  const state = bucketState(bucket, isStale);
  const pause = providerUsagePause({ scheduling: bucket.scheduling }, zh);
  const totals = bucket.api_price?.all_time;
  const queriedAt = Date.parse(bucket.queried_at);
  // A scoped adapter prevents parent money/balances or unrelated tiers leaking into the summary.
  const priceUsage = { provider_id: usage.provider_id, tiers: bucket.tiers, api_price: bucket.api_price };
  return <article data-quota-bucket={identity(bucket)} aria-label={`${bucket.label} ${zh ? "配额桶" : "quota bucket"}`}
    className="min-w-0 space-y-3 rounded-xl border border-slate-200/80 bg-white/60 p-3">
    <header className="space-y-1.5">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h3 className="text-xs font-semibold text-slate-800">{bucket.label || (zh ? "未命名配额桶" : "Unnamed quota bucket")}</h3>
        <span data-bucket-status={state} className={`rounded-full px-2 py-0.5 text-[10px] font-semibold ${stateClass(state)}`}>
          {stateLabel(state, zh)}{isStale && ` · ${zh ? "旧快照" : "stale"}`}
        </span>
      </div>
      <p className="break-words font-mono text-[10px] text-slate-500" title={bucket.models.join(", ")}>
        {bucket.models.length ? bucket.models.join(" · ") : (zh ? "模型绑定未知" : "Model binding unknown")}
      </p>
      <p className="text-[10px] text-slate-400">
        {sourceLabel(bucket.binding.source, zh)} · {bucket.binding.status === "resolved" ? (zh ? "已解析" : "Resolved") : (zh ? "绑定待确认" : "Binding unresolved")} · r{bucket.binding.revision}
      </p>
    </header>
    {pause && <p className="rounded-lg bg-red-50 px-2 py-1.5 text-[10px] text-red-700">
      {pause.message}{pause.quotaExhausted && bucket.scheduling.blocking_tiers.length > 0 && ` · ${bucket.scheduling.blocking_tiers.map((name) => tierLabel(name, zh)).join(", ")}`}
    </p>}
    {isStale && <p className="flex items-start gap-1 text-[10px] text-amber-700"><AlertCircle className="mt-0.5 h-3 w-3 shrink-0" />
      {zh ? "保留上次观察；旧数据不代表额度恢复，暂不估算容量。" : "Last observation retained; stale data does not prove recovery. Capacity projection is withheld."}
    </p>}
    <div className="space-y-2.5">
      {bucket.tiers.map((tier) => {
        const used = tier.used_percent != null && Number.isFinite(tier.used_percent)
          ? Math.min(100, Math.max(0, tier.used_percent)) : null;
        const reset = tier.resets_at ? Date.parse(tier.resets_at) : NaN;
        const countdown = countdownStr(tier.resets_at, now);
        // Do not infer fixed-duration semantics from a reset timestamp. The backend owns the cycle.
        return <div key={JSON.stringify([bucket.scope_id, bucket.id, tier.name])} data-bucket-window={JSON.stringify([bucket.scope_id, bucket.id, tier.name])} className="space-y-1">
          <div className="flex justify-between gap-2 text-[11px]">
            <span className="font-medium text-slate-500">{tierLabel(tier.name, zh)}</span>
            <span className={`font-semibold tabular-nums ${used == null ? "text-slate-400" : utilizationColor(used)}`}>
              {used == null ? (zh ? "未知" : "Unknown") : `${Math.round(used)}% ${zh ? "已用" : "used"}`}
            </span>
          </div>
          {used != null ? <UsageProgress usedPercent={used} isZh={zh} label={`${bucket.label} ${tierLabel(tier.name, zh)} ${zh ? "已用额度" : "used quota"}`} />
            : <div className="h-2 rounded-full bg-slate-100" aria-label={zh ? "用量未知" : "Usage unknown"} />}
          <p className="flex items-center gap-1 text-[10px] text-slate-400">
            <Clock className="h-3 w-3" />{zh ? "重置" : "Reset"}: {Number.isFinite(reset)
              ? <time dateTime={tier.resets_at!} title={new Date(reset).toLocaleString(locale)}>{countdown ?? new Date(reset).toLocaleString(locale)}</time>
              : (zh ? "未知" : "Unknown")}
          </p>
        </div>;
      })}
      {!bucket.tiers.length && <p className="text-[11px] text-slate-400">{zh ? "暂无窗口观察" : "No window observations"}</p>}
    </div>
    <div className="flex flex-wrap gap-x-4 gap-y-1 border-t border-slate-200/60 pt-2 text-[10px] text-slate-500">
      <span>{zh ? "保留请求" : "Retained attempts"} <strong className="font-semibold tabular-nums text-slate-700">{totals ? totals.request_count.toLocaleString(locale) : "—"}</strong></span>
      <span>{zh ? "已记录 Token" : "Recorded tokens"} <strong className="font-semibold tabular-nums text-slate-700">{totals ? formatTokenCount(totals.total_tokens) : "—"}</strong></span>
    </div>
    <ProviderApiPriceSummary usage={priceUsage} now={now} stale={isStale} loading={loading} locale={locale}
      bucketLabel={bucket.label} scopeKey={identity(bucket)} projectionAllowed={bucket.binding.status === "resolved" && bucket.source !== "catalog"
        && !["unknown_mapping", "conflicting_binding"].includes(bucket.scheduling.reason ?? "")} />
    {bucket.api_price?.all_time_coverage && <div className="space-y-1 text-[10px] text-slate-500">
      <p>{zh ? "Token（入 / 出 / 缓存）" : "Tokens (in / out / cache)"}: {formatTokenCount(bucket.api_price.all_time_coverage.input_tokens)} / {formatTokenCount(bucket.api_price.all_time_coverage.output_tokens)} / {formatTokenCount(bucket.api_price.all_time_coverage.cache_read_tokens)}</p>
      <p>{zh ? "缺少用量" : "Missing usage"}: {bucket.api_price.all_time_coverage.missing_usage_request_count} · {zh ? "未计价" : "Unpriced"}: {bucket.api_price.all_time_coverage.unpriced_request_count} · {zh ? "推断归属" : "Inferred"}: {bucket.api_price.all_time_coverage.inferred_request_count}</p>
    </div>}
    {Boolean(bucket.api_price?.models?.length) && <details className="text-[10px] text-slate-500"><summary className="cursor-pointer">{zh ? "精确模型计价明细" : "Exact-model price breakdown"}</summary>
      <ul className="mt-1 space-y-1">{bucket.api_price!.models!.map((model) => <li key={model.model} className="flex flex-wrap justify-between gap-2">
        <span className="break-all font-mono">{model.model}</span><span className="tabular-nums" title={retainedUsagePriceDisplay(model.all_time, zh, locale).title}>{retainedUsagePriceDisplay(model.all_time, zh, locale).text} · {model.all_time.request_count} {zh ? "次" : "attempts"}</span>
      </li>)}</ul>
    </details>}
    <footer className="flex flex-wrap items-center justify-between gap-2 border-t border-slate-200/60 pt-2 text-[10px] text-slate-400">
      <span title={bucket.queried_at}>{sourceLabel(bucket.source, zh)} · {Number.isFinite(queriedAt) ? <time dateTime={bucket.queried_at}>{new Date(queriedAt).toLocaleString(locale)}</time> : (zh ? "观察时间未知" : "Observation time unknown")}</span>
      {onDetails && <button type="button" className="inline-flex cursor-pointer items-center gap-0.5 font-medium text-sky-700 hover:underline"
        onClick={(event) => { event.stopPropagation(); onDetails({ quota_bucket: bucket.id, quota_scope: bucket.scope_id }); }}>
        {zh ? "查看桶详情" : "Bucket details"}<ArrowUpRight className="h-3 w-3" />
      </button>}
    </footer>
    {bucket.constraint_ids.length > 0 && <details className="text-[10px] text-slate-400"><summary className="cursor-pointer">{zh ? "适用约束" : "Applicable constraints"} · {bucket.constraint_ids.length}</summary>
      <p className="mt-1 break-all font-mono">{bucket.constraint_ids.join(" · ")}</p>
    </details>}
  </article>;
}

/** One provider, independently scoped quota and price subcards, one provider grand total. */
export function ProviderQuotaBuckets({ usage, now, stale, loading, locale = "en-US", onDetails }: {
  usage: ProviderUsage; now: number; stale: boolean; loading: boolean; locale?: Locale;
  onDetails?: (filter: QuotaBucketFilter) => void;
}) {
  const zh = locale === "zh-CN";
  const buckets = (usage.quota_buckets ?? []).filter((bucket) => !["shared_constraint", "window_constraint"].includes(bucket.binding.status));
  const shared = (usage.quota_buckets ?? []).filter((bucket) => bucket.binding.status === "shared_constraint");
  if (!buckets.length && !shared.length) return null;
  const unassigned = usage.api_price?.unassigned;
  const price = unassigned ? retainedUsagePriceDisplay(unassigned, zh, locale) : null;
  const historical = (usage.api_price?.retained_buckets ?? []).filter((retained) => !buckets.some((bucket) => bucket.id === retained.bucket_id && bucket.scope_id === retained.scope_id));
  return <div className="space-y-3">
    <div className="grid grid-cols-[repeat(auto-fit,minmax(min(100%,20rem),1fr))] gap-3">
      {buckets.map((bucket) => <BucketCard key={identity(bucket)} usage={usage} bucket={bucket} now={now} stale={stale}
        loading={loading} locale={locale} onDetails={onDetails} />)}
    </div>
    {shared.length > 0 && <details className="rounded-lg border border-slate-200/70 p-2 text-[10px] text-slate-500">
      <summary className="cursor-pointer">{zh ? "共享约束 / 绑定资源（非独立容量）" : "Shared constraints / binding resources (not independent capacity)"} · {shared.length}</summary>
      <ul className="mt-2 space-y-1">{shared.map((bucket) => <li key={identity(bucket)} className="break-words">
        <strong>{bucket.label}</strong> · {sourceLabel(bucket.source, zh)} · {bucket.scheduling.status === "quota_exhausted" ? (zh ? "阻塞中" : "Blocking") : (zh ? "约束观察" : "Constraint observation")}
        {bucket.stale && ` · ${zh ? "旧数据" : "Stale"}`}
        {bucket.tiers.map((tier) => ` · ${tierLabel(tier.name, zh)}: ${Number.isFinite(tier.used_percent) ? `${tier.used_percent}%` : (zh ? "未知" : "Unknown")}`).join("")}
      </li>)}</ul>
    </details>}
    <ProviderApiPriceSummary usage={usage} now={now} stale={stale} loading={loading} locale={locale} totalOnly />
    {historical.length > 0 && <details className="text-[10px] text-slate-500"><summary className="cursor-pointer">{zh ? "历史配额桶（非当前额度）" : "Retired bucket history (not current capacity)"} · {historical.length}</summary>
      <ul className="mt-1 space-y-1">{historical.map((retained) => <li key={JSON.stringify([retained.scope_id, retained.bucket_id])} className="flex flex-wrap justify-between gap-2">
        <span className="break-all font-mono">{retained.models.map((model) => model.model).join(" · ") || retained.bucket_id}</span>
        <span title={retainedUsagePriceDisplay(retained.all_time, zh, locale).title}>{retainedUsagePriceDisplay(retained.all_time, zh, locale).text} · {retained.all_time.request_count} {zh ? "次尝试" : "attempts"}</span>
        {onDetails && <button type="button" className="cursor-pointer text-sky-700 hover:underline" onClick={(event) => { event.stopPropagation(); onDetails({ quota_bucket: retained.bucket_id, quota_scope: retained.scope_id }); }}>{zh ? "历史详情" : "History details"}</button>}
      </li>)}</ul>
    </details>}
    <div className="flex flex-wrap items-center justify-between gap-2 text-[10px] text-slate-500" data-unassigned-price>
      <span title={price?.title}>{zh ? "未归属" : "Unassigned"}: <strong className="font-semibold tabular-nums">{price?.text ?? "—"}</strong>
        {unassigned && ` · ${unassigned.request_count.toLocaleString(locale)} ${zh ? "次尝试" : "attempts"}`}</span>
      {onDetails && <button type="button" className="cursor-pointer font-medium text-sky-700 hover:underline"
        onClick={(event) => { event.stopPropagation(); onDetails({ quota_bucket: "unassigned" }); }}>{zh ? "查看未归属" : "Unassigned details"}</button>}
    </div>
  </div>;
}
