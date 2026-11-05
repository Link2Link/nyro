import { useState } from "react";
import { AlertCircle, ArrowUpRight, Clock } from "lucide-react";
import type { Locale } from "@/lib/i18n";
import { hasStructuredQuotaBuckets, isGoogleFamilyDisplayBucket, isPrimaryQuotaBucket, providerQuotaDisplayBuckets } from "@/lib/quota-bucket-display";
import { tierLabel, providerUsagePause } from "@/lib/provider-usage";
import { formatTokenCount } from "@/lib/format";
import { retainedUsagePriceDisplay } from "@/lib/token-usage";
import { countdownStr, utilizationColor } from "@/lib/usage-pace";
import type { ProviderUsage, QuotaBucket, QuotaBucketFilter } from "@/lib/types";
import { UsageProgress } from "@/components/usage-progress";
import { ProviderApiPriceSummary } from "@/components/provider-api-price-summary";

type BucketState = "available" | "observed" | "exhausted" | "unavailable" | "unknown";
const identity = (bucket: QuotaBucket) => JSON.stringify([bucket.scope_id, bucket.id]);
const bucketStale = (bucket: QuotaBucket, now: number, refreshFailed: boolean) => {
  const observed = Date.parse(bucket.queried_at);
  return refreshFailed || bucket.stale || !Number.isFinite(observed) || now - observed >= 5 * 60_000;
};
const accountUnavailable = (reason?: string | null) => ["account_unavailable", "no_available_account", "auth_required", "auth_error",
  "authentication_failed", "disabled", "disconnected"].includes(reason ?? "");
const unavailableOwner = (usage?: Pick<ProviderUsage, "is_available" | "scheduling">) => usage?.is_available === false
  || accountUnavailable(usage?.scheduling?.reason);
const bucketTierLabel = (name: string, zh: boolean) => name === "catalog"
  ? (zh ? "模型目录用量" : "Model catalog usage") : tierLabel(name, zh);
// Observation success is independent of binding/admission. Only server-projected
// Google families can be observed here; raw labels/models never establish a family.
function bucketState(bucket: QuotaBucket, stale: boolean, ownerUnavailable = false): BucketState {
  if (ownerUnavailable || accountUnavailable(bucket.scheduling?.reason)) return "unavailable";
  if (bucket.scheduling?.status === "quota_exhausted") return "exhausted";
  if (stale) return "unknown";
  if (isGoogleFamilyDisplayBucket(bucket) && bucket.binding.status === "unresolved" && bucket.models.length > 0
    && bucket.scheduling?.status === "eligible" && ["", "unknown_mapping", "unresolved_binding"].includes(bucket.scheduling.reason ?? "")
    && bucket.tiers.length > 0 && bucket.tiers.every((tier) => Number.isFinite(tier.used_percent)
      && tier.used_percent >= 0 && tier.used_percent <= 100)) return "observed";
  if (bucket.binding.status !== "resolved" || !bucket.models.length || bucket.scheduling?.status !== "eligible"
    || bucket.source === "catalog" || ["unknown_mapping", "conflicting_binding"].includes(bucket.scheduling.reason ?? "")) return "unknown";
  return "available";
}
const stateClass = (state: string) => state === "available" ? "bg-emerald-50 text-emerald-700"
  : state === "exhausted" || state === "unavailable" ? "bg-red-50 text-red-700" : "bg-amber-50 text-amber-700";
const stateLabel = (state: BucketState, zh: boolean) => state === "available" ? (zh ? "可调度" : "Available")
  : state === "observed" ? (zh ? "用量已更新" : "Usage updated")
    : state === "unavailable" ? (zh ? "账号不可用" : "Account unavailable")
      : state === "exhausted" ? (zh ? "已暂停" : "Paused") : (zh ? "状态未知" : "Unknown");
const sourceLabel = (source: string, zh: boolean) => {
  const labels: Record<string, [string, string]> = {
    configured: ["Configured", "显式绑定"], official: ["Official", "官方绑定"],
    adapter: ["Validated adapter", "已验证适配规则"], google_exact_model_v1: ["Validated Google binding", "已验证 Google 绑定"],
    google_subscription_resources_v1: ["Validated Google subscription resources", "已验证 Google 订阅资源"],
    google_named_family_v1: ["Validated Google named family", "已验证 Google 命名模型族"],
    google_family_display_v1: ["Google family display", "Google 模型族展示"],
    catalog_exact_model_v1: ["Catalog exact-model binding", "目录精确模型绑定"], enforced_summary: ["Enforced summary", "强制额度摘要"],
    model_catalog: ["Model catalog", "模型目录"], catalog: ["Model catalog", "模型目录"],
    unresolved: ["Unresolved", "未解析"], unknown: ["Unknown", "未知"],
  };
  return labels[source]?.[zh ? 1 : 0] ?? source;
};

export function QuotaBucketAvailability({ buckets, stale = false, isZh = false, now, expectedCount, owner }: {
  buckets: QuotaBucket[]; stale?: boolean; isZh?: boolean; now?: number; expectedCount?: number;
  owner?: Pick<ProviderUsage, "is_available" | "scheduling">;
}) {
  const [initialNow] = useState(() => Date.now());
  const observedNow = now ?? initialNow;
  const ownerUnavailable = unavailableOwner(owner);
  const primary = buckets.filter((bucket) => isPrimaryQuotaBucket(bucket) && (expectedCount == null || isGoogleFamilyDisplayBucket(bucket)));
  const states = primary.map((bucket) => bucketState(bucket, bucketStale(bucket, observedNow, stale), ownerUnavailable));
  const available = states.filter((state) => state === "available").length;
  const observed = states.filter((state) => state === "observed").length;
  const exhausted = states.filter((state) => state === "exhausted").length;
  const unavailable = states.filter((state) => state === "unavailable").length;
  // A fixed family count is meaningful only within one entitlement owner, never across scopes.
  const scoped = expectedCount == null || (new Set(primary.map((bucket) => bucket.scope_id)).size <= 1 && states.length <= expectedCount);
  const complete = scoped && states.length > 0 && (expectedCount == null || states.length === expectedCount);
  const state = !scoped ? "unknown" : ownerUnavailable || (complete && unavailable === states.length) ? "unavailable"
    : complete && available === states.length ? "available"
      : complete && exhausted === states.length ? "exhausted"
        : available > 0 ? "partial" : observed > 0 ? "observed" : "unknown";
  const count = state === "observed" ? available + observed : available;
  const old = primary.some((bucket) => bucketStale(bucket, observedNow, stale));
  const label = state === "available" ? (isZh ? "全部可用" : "Available")
    : state === "partial" ? (isZh ? "部分可用" : "Partially available")
      : state === "observed" ? (isZh ? "用量已更新" : "Usage updated")
        : state === "unavailable" ? (isZh ? "账号不可用" : "Account unavailable")
          : state === "exhausted" ? (isZh ? "全部暂停" : "Exhausted / unavailable")
            : stale ? (isZh ? "刷新失败" : "Refresh failed") : old ? (isZh ? "旧数据" : "Stale") : (isZh ? "状态未知" : "Unknown");
  return <span data-quota-availability={state} className={`inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-[10px] font-semibold ${stateClass(state)}`}>
    <span aria-hidden="true" className="h-1.5 w-1.5 rounded-full bg-current" />
    {label}{scoped && count > 0 && ` · ${count}/${expectedCount ?? states.length}`}
    {scoped && available > 0 && observed > 0 && ` · ${isZh ? "用量已更新" : "Usage updated"} · ${available + observed}/${expectedCount ?? states.length}`}
    {scoped && observed > 0 && exhausted > 0 && ` · ${isZh ? "已暂停" : "Paused"} ${exhausted}`}
    {scoped && observed > 0 && unavailable > 0 && ` · ${isZh ? "账号不可用" : "Account unavailable"} ${unavailable}`}
    {state !== "unknown" && old && ` · ${stale ? (isZh ? "刷新失败" : "Refresh failed") : (isZh ? "旧数据" : "Stale")}`}
  </span>;
}

/** Display-family IDs are not attribution filters. Only same-scope raw accounting sources can open detail. */
export function QuotaBucketSourceLinks({ bucket, resources, onDetails, isZh = false }: {
  bucket: QuotaBucket; resources: QuotaBucket[]; onDetails?: (filter: QuotaBucketFilter) => void; isZh?: boolean;
}) {
  const sources = resources.filter((source) => source.scope_id === bucket.scope_id && bucket.constraint_ids.includes(source.id)
    && !isGoogleFamilyDisplayBucket(source) && source.binding.status !== "shared_constraint" && source.models.length > 0);
  if (!onDetails || !sources.length) return null;
  return <div className="space-y-1" data-quota-source-links>
    <p>{isZh ? "原始归属桶详情" : "Source bucket details"}</p>
    <p className="text-slate-400">{isZh ? "按原始日志归属查询；窗口资源可能没有归属记录。" : "Uses original log attribution; a window resource may have no attributed attempts."}</p>
    <ul className="space-y-1">{sources.map((source) => <li key={identity(source)}>
      <button type="button" data-quota-detail={identity(source)} className="inline-flex cursor-pointer items-center gap-0.5 text-sky-700 hover:underline"
        onClick={(event) => { event.stopPropagation(); onDetails({ quota_bucket: source.id, quota_scope: source.scope_id }); }}>
        {source.label || source.id}<ArrowUpRight className="h-3 w-3 shrink-0" />
      </button>
    </li>)}</ul>
  </div>;
}

function BucketCard({ usage, bucket, now, stale, loading, locale, onDetails }: {
  usage: ProviderUsage; bucket: QuotaBucket; now: number; stale: boolean; loading: boolean;
  locale: Locale; onDetails?: (filter: QuotaBucketFilter) => void;
}) {
  const zh = locale === "zh-CN";
  const isStale = bucketStale(bucket, now, stale);
  const state = bucketState(bucket, isStale, unavailableOwner(usage));
  const pause = providerUsagePause({ scheduling: bucket.scheduling }, zh);
  const statusLabel = state === "unknown" && stale ? (zh ? "刷新失败" : "Refresh failed")
    : state === "unknown" && isStale ? (zh ? "旧数据" : "Stale") : stateLabel(state, zh);
  const totals = bucket.api_price?.all_time;
  const queriedAt = Date.parse(bucket.queried_at);
  // A scoped adapter prevents parent money/balances or unrelated tiers leaking into the summary.
  const priceUsage = { provider_id: usage.provider_id, tiers: bucket.tiers, api_price: bucket.api_price };
  const family = usage.kind === "google_subscription" && isGoogleFamilyDisplayBucket(bucket);
  const bindingDetails = <>
    <p className="break-words font-mono text-[10px] text-slate-500" title={bucket.models.join(", ")}>
      {bucket.models.length ? bucket.models.join(" · ") : (zh ? "模型绑定未知" : "Model binding unknown")}
    </p>
    <p className="text-[10px] text-slate-400">
      {sourceLabel(bucket.binding.source, zh)} · {bucket.binding.status === "resolved" ? (zh ? "已解析" : "Resolved") : (zh ? "绑定待确认" : "Binding unresolved")} · r{bucket.binding.revision}
    </p>
  </>;
  const coverageDetails = bucket.api_price?.all_time_coverage && <div className="space-y-1 text-[10px] text-slate-500">
    <p>{zh ? "Token（入 / 出 / 缓存）" : "Tokens (in / out / cache)"}: {formatTokenCount(bucket.api_price.all_time_coverage.input_tokens)} / {formatTokenCount(bucket.api_price.all_time_coverage.output_tokens)} / {formatTokenCount(bucket.api_price.all_time_coverage.cache_read_tokens)}</p>
    <p>{zh ? "缺少用量" : "Missing usage"}: {bucket.api_price.all_time_coverage.missing_usage_request_count} · {zh ? "未计价" : "Unpriced"}: {bucket.api_price.all_time_coverage.unpriced_request_count} · {zh ? "推断归属" : "Inferred"}: {bucket.api_price.all_time_coverage.inferred_request_count}</p>
  </div>;
  const exactPrices = Boolean(bucket.api_price?.models?.length) && <details className="text-[10px] text-slate-500"><summary className="cursor-pointer">{zh ? "精确模型计价明细" : "Exact-model price breakdown"}</summary>
    <ul className="mt-1 space-y-1">{bucket.api_price!.models!.map((model) => <li key={model.model} className="flex flex-wrap justify-between gap-2">
      <span className="break-all font-mono">{model.model}</span><span className="tabular-nums" title={retainedUsagePriceDisplay(model.all_time, zh, locale).title}>{retainedUsagePriceDisplay(model.all_time, zh, locale).text} · {model.all_time.request_count} {zh ? "次" : "attempts"}</span>
    </li>)}</ul>
  </details>;
  const constraints = bucket.constraint_ids.length > 0 && <details className="text-[10px] text-slate-400"><summary className="cursor-pointer">{zh ? "适用约束" : "Applicable constraints"} · {bucket.constraint_ids.length}</summary>
    <p className="mt-1 break-all font-mono">{bucket.constraint_ids.join(" · ")}</p>
  </details>;
  return <article data-quota-bucket={identity(bucket)} aria-label={`${bucket.label} ${zh ? "配额桶" : "quota bucket"}`}
    className="min-w-0 space-y-3 rounded-xl border border-slate-200/80 bg-white/60 p-3">
    <header className="space-y-1.5">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h3 className="text-xs font-semibold text-slate-800">{bucket.label || (zh ? "未命名配额桶" : "Unnamed quota bucket")}</h3>
        <span data-bucket-status={state} className={`rounded-full px-2 py-0.5 text-[10px] font-semibold ${stateClass(state)}`}>
          {statusLabel}{isStale && state !== "unknown" && ` · ${zh ? "旧快照" : "stale"}`}
        </span>
      </div>
      {!family && bindingDetails}
    </header>
    {state === "observed" && <p role="status" className="rounded-lg bg-amber-50 px-2 py-1.5 text-[10px] text-amber-700">
      {zh ? "配额绑定待确认；已取得最新用量观察，不代表强制配额已解析或可调度。"
        : "Quota binding pending confirmation; fresh usage was observed, but enforced quota and routing availability are not confirmed."}
    </p>}
    {bucket.tiers.some((tier) => tier.name === "catalog") && <p className="text-[10px] text-slate-500">
      {zh ? "来源：模型目录；用量周期未知，不用于推断周 / 5 小时配额或容量。"
        : "Source: model catalog. The usage window is unknown; no weekly / 5h quota or capacity is inferred."}
    </p>}
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
            <span className="font-medium text-slate-500">{bucketTierLabel(tier.name, zh)}</span>
            <span className={`font-semibold tabular-nums ${used == null ? "text-slate-400" : utilizationColor(used)}`}>
              {used == null ? (zh ? "未知" : "Unknown") : `${Math.round(used)}% ${zh ? "已用" : "used"}`}
            </span>
          </div>
          {used != null ? <UsageProgress usedPercent={used} isZh={zh} label={`${bucket.label} ${bucketTierLabel(tier.name, zh)} ${zh ? "已用额度" : "used quota"}`} />
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
    {!family && <>{coverageDetails}{exactPrices}</>}
    {family && <details data-bucket-diagnostics className="space-y-2 text-[10px] text-slate-500">
      <summary className="cursor-pointer">{zh ? "模型与诊断详情" : "Models and diagnostics"}</summary>
      {bindingDetails}{coverageDetails}{exactPrices}{constraints}
      <QuotaBucketSourceLinks bucket={bucket} resources={usage.quota_buckets ?? []} onDetails={onDetails} isZh={zh} />
    </details>}
    <footer className="flex flex-wrap items-center justify-between gap-2 border-t border-slate-200/60 pt-2 text-[10px] text-slate-400">
      <span title={bucket.queried_at}>{!family && <>{sourceLabel(bucket.source, zh)} · </>}{Number.isFinite(queriedAt) ? <time dateTime={bucket.queried_at}>{new Date(queriedAt).toLocaleString(locale)}</time> : (zh ? "观察时间未知" : "Observation time unknown")}</span>
      {!isGoogleFamilyDisplayBucket(bucket) && onDetails && <button type="button" className="inline-flex cursor-pointer items-center gap-0.5 font-medium text-sky-700 hover:underline"
        onClick={(event) => { event.stopPropagation(); onDetails({ quota_bucket: bucket.id, quota_scope: bucket.scope_id }); }}>
        {zh ? "查看桶详情" : "Bucket details"}<ArrowUpRight className="h-3 w-3" />
      </button>}
    </footer>
    {!family && constraints}
  </article>;
}

/** One provider, independently scoped quota and price subcards, one provider grand total. */
export function ProviderQuotaBuckets({ usage, now, stale, loading, locale = "en-US", onDetails }: {
  usage: ProviderUsage; now: number; stale: boolean; loading: boolean; locale?: Locale;
  onDetails?: (filter: QuotaBucketFilter) => void;
}) {
  const zh = locale === "zh-CN";
  const google = usage.kind === "google_subscription";
  const buckets = providerQuotaDisplayBuckets(usage);
  const raw = (usage.quota_buckets ?? []).filter((bucket) => !isGoogleFamilyDisplayBucket(bucket));
  const observations = google ? raw : raw.filter((bucket) => bucket.binding.status === "shared_constraint");
  if (!buckets.length && !observations.length && !(google && hasStructuredQuotaBuckets(usage))) return null;
  const unassigned = usage.api_price?.unassigned;
  const price = unassigned ? retainedUsagePriceDisplay(unassigned, zh, locale) : null;
  // Display families do not replace immutable raw accounting identities or make current raw sources retired.
  const accountingBuckets = google ? raw : buckets;
  const historical = (usage.api_price?.retained_buckets ?? []).filter((retained) => !accountingBuckets.some((bucket) => bucket.id === retained.bucket_id && bucket.scope_id === retained.scope_id));
  const observationDetails = observations.length > 0 && <details className="rounded-lg border border-slate-200/70 p-2 text-[10px] text-slate-500">
    <summary className="cursor-pointer">{google
      ? (zh ? "原始配额观察（仅诊断）" : "Raw quota observations (diagnostics only)")
      : (zh ? "共享约束 / 绑定资源（非独立容量）" : "Shared constraints / binding resources (not independent capacity)")} · {observations.length}</summary>
    <ul className="mt-2 space-y-1">{observations.map((bucket) => <li key={identity(bucket)} data-quota-observation={identity(bucket)} className="break-words">
      <strong>{bucket.label}</strong> · {sourceLabel(bucket.source, zh)} · {bucket.scheduling.status === "quota_exhausted" ? (zh ? "阻塞中" : "Blocking") : (zh ? "约束观察" : "Constraint observation")}
      {bucket.stale && ` · ${zh ? "旧数据" : "Stale"}`}
      {bucket.tiers.map((tier) => ` · ${bucketTierLabel(tier.name, zh)}: ${Number.isFinite(tier.used_percent) ? `${tier.used_percent}%` : (zh ? "未知" : "Unknown")}`).join("")}
      {google && <span className="block break-all font-mono">{bucket.models.join(" · ") || bucket.id}</span>}
    </li>)}</ul>
  </details>;
  const retainedDetails = <>
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
  </>;
  return <div className="space-y-3">
    {buckets.length > 0 ? <div className="grid grid-cols-[repeat(auto-fit,minmax(min(100%,20rem),1fr))] gap-3">
      {buckets.map((bucket) => <BucketCard key={identity(bucket)} usage={usage} bucket={bucket} now={now} stale={stale}
        loading={loading} locale={locale} onDetails={onDetails} />)}
    </div> : google && <p role="status" className="text-[11px] text-amber-700">{zh
      ? "尚无目标模型族的配额观察；可用性未知。"
      : "No quota observations for the target model families; availability is unknown."}</p>}
    {!google && observationDetails}
    <ProviderApiPriceSummary usage={usage} now={now} stale={stale} loading={loading} locale={locale} totalOnly />
    {google ? <details data-provider-quota-diagnostics className="space-y-2 text-[10px] text-slate-500">
      <summary className="cursor-pointer">{zh ? "配额诊断与历史" : "Quota diagnostics and history"}</summary>
      {observationDetails}{retainedDetails}
    </details> : retainedDetails}
  </div>;
}
