import { useEffect, useState } from "react";
import { useQueries, useQuery } from "@tanstack/react-query";
import { Clock, RefreshCw, AlertCircle, CirclePause } from "lucide-react";
import { backend } from "@/lib/backend";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { isKiroOverage, providerUsageSupported, providerUsageTitle, summarizeGroupUsage, tierLabel } from "@/lib/provider-usage";
import { isDevinProvider } from "@/lib/native-provider";
import { useLocale } from "@/lib/i18n";
import { accountStateLabel, poolQuotaWindows, type AccountPoolView } from "@/lib/devin-account-pool";
import { poolSteadyPacePercent, steadyPacePercent, utilizationBarClass, utilizationColor } from "@/lib/usage-pace";
import { useUsageClock } from "@/lib/use-usage-clock";
import { UsageProgress } from "@/components/usage-progress";
import type {
  Provider,
  ProviderUsage,
  ProviderUsageBalance,
  ProviderUsageCredentials,
  ProviderUsageSpend,
  ProviderUsageTier,
} from "@/lib/types";

/**
 * Coding-plan usage footer for provider cards, styled after cc-switch's
 * SubscriptionQuotaFooter: per-tier progress bars with the used percentage
 * color-coded by utilization (green < 70%, orange >= 70%, red >= 90%) and a
 * reset countdown like "2h30m" / "3d12h".
 */

/** Compact countdown like "2h30m" / "3d12h" (cc-switch countdownStr). */
function countdownStr(resetsAt?: string | null): string | null {
  if (!resetsAt) return null;
  const diffMs = new Date(resetsAt).getTime() - Date.now();
  if (diffMs <= 0) return null;

  const hours = Math.floor(diffMs / (1000 * 60 * 60));
  const minutes = Math.floor((diffMs % (1000 * 60 * 60)) / (1000 * 60));

  if (hours > 24) {
    const days = Math.floor(hours / 24);
    return `${days}d${hours % 24}h`;
  }
  if (hours > 0) return `${hours}h${minutes}m`;
  return `${minutes}m`;
}

function formatRelativeTime(timestamp: string, now: number, isZh: boolean): string {
  const diff = Math.floor((now - new Date(timestamp).getTime()) / 1000);
  if (diff < 60) return isZh ? "刚刚" : "just now";
  if (diff < 3600) return isZh ? `${Math.floor(diff / 60)} 分钟前` : `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return isZh ? `${Math.floor(diff / 3600)} 小时前` : `${Math.floor(diff / 3600)}h ago`;
  return isZh ? `${Math.floor(diff / 86400)} 天前` : `${Math.floor(diff / 86400)}d ago`;
}

function TierBar({
  tier,
  isZh,
  now,
}: {
  tier: ProviderUsageTier;
  isZh: boolean;
  /** Tracked so the steady-pace marker re-evaluates on the 30s tick. */
  now: number;
}) {
  const countdown = countdownStr(tier.resets_at);
  const used = Math.min(Math.max(tier.used_percent, 0), 100);
  const pace = steadyPacePercent(tier, now);
  return (
    <div className="flex items-center gap-3 text-xs">
      <span className="min-w-14 max-w-36 shrink-0 font-medium text-slate-500" title={tier.name}>
        {tierLabel(tier.name, isZh)}
      </span>
      <UsageProgress usedPercent={used} pacePercent={pace} isZh={isZh}
        label={`${tierLabel(tier.name, isZh)} ${isZh ? "已用额度" : "used quota"}`} />
      <span className={`w-10 shrink-0 text-right font-semibold tabular-nums ${utilizationColor(used)}`}>
        {Math.round(used)}%
      </span>
      <span
        className="flex w-14 shrink-0 items-center gap-0.5 text-[10px] text-slate-400"
        title={tier.resets_at ?? undefined}
      >
        {countdown ? (
          <>
            <Clock className="h-3 w-3" />
            {countdown}
          </>
        ) : null}
      </span>
    </div>
  );
}

function SpendRow({ spend, isZh }: { spend: ProviderUsageSpend; isZh: boolean }) {
  const symbol = spend.currency === "CNY" ? "¥" : spend.currency === "USD" ? "$" : "";
  const label =
    spend.name === "today"
      ? isZh
        ? "今日"
        : "Today"
      : spend.name === "month"
        ? isZh
          ? "本月"
          : "Month"
        : spend.name;
  return (
    <div className="flex items-center justify-between gap-3 text-xs">
      <span className="w-14 shrink-0 font-medium text-slate-500">{label}</span>
      <span className="flex-1 font-semibold tabular-nums text-slate-700">
        {symbol}
        {spend.amount.toFixed(2)}
      </span>
      <span className="w-14 shrink-0" />
    </div>
  );
}

function BalanceRow({
  balance,
  isAvailable,
  isZh,
  label,
}: {
  balance: ProviderUsageBalance;
  isAvailable: boolean;
  isZh: boolean;
  /** Overrides the generic "Balance" label (e.g. Devin's extra usage). */
  label?: string;
}) {
  const symbol = balance.currency === "CNY" ? "¥" : balance.currency === "USD" ? "$" : "";
  const fmt = (value: number) => value.toFixed(2);
  // cc-switch colors balance rows by availability; an empty balance is never
  // painted as "available" green.
  const color = !isAvailable ? "text-red-500" : balance.total > 0 ? "text-emerald-600" : "text-slate-400";
  return (
    <div className="flex items-center justify-between gap-3 text-xs">
      <span
        className={`shrink-0 font-medium text-slate-500 ${label ? "min-w-14 max-w-36" : "w-14"}`}
        title={label}
      >
        {label ?? (isZh ? "余额" : "Balance")}
      </span>
      <div className="flex flex-1 flex-wrap items-center gap-x-2">
        <span className={`font-semibold tabular-nums ${color}`}>
          {symbol}
          {fmt(balance.total)}
        </span>
        <span className="text-[10px] text-slate-400">
          {balance.currency}
          {balance.granted > 0 ? ` (${isZh ? "充值" : "topped-up"} ${symbol}${fmt(balance.topped_up)}${isZh ? " + 赠送" : " + granted"} ${symbol}${fmt(balance.granted)})` : ""}
        </span>
      </div>
      {!isAvailable && (
        <span className="shrink-0 rounded bg-red-50 px-1.5 py-0.5 text-[10px] font-semibold text-red-500">
          {isZh ? "不可用" : "Unavailable"}
        </span>
      )}
    </div>
  );
}

/** Actual percentages stay backend-owned; the marker is a capacity-weighted time estimate. */
export function DevinPoolUsage({ pool, detailed = false }: { pool: AccountPoolView; detailed?: boolean }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const now = useUsageClock();
  const windows = poolQuotaWindows(pool);
  if (!pool.config) return null;
  return <div className="space-y-2 rounded-lg border border-slate-200/70 bg-white/40 px-3 py-2 text-xs">
    {detailed && <>
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
      <strong>Devin</strong>
      {(pool.counts.pro > 0 || pool.counts.max > 0) && <span className="rounded bg-sky-50 px-1.5 py-0.5 font-semibold text-sky-700">
        {pool.counts.pro > 0 && pool.counts.max > 0 ? "Mixed" : pool.counts.max > 0 ? "Max" : "Pro"}
      </span>}
      <span>{isZh ? `${pool.counts.total} 个账号 · ${pool.counts.available} 个可用` : `${pool.counts.total} accounts · ${pool.counts.available} available`}</span>
      {pool.quota.estimated && <span className="text-amber-700">{isZh ? "容量估算" : "Estimated capacity"}</span>}
    </div>
    <p className="text-slate-500">Pro / Team Full Seat: {pool.counts.pro} · Max: {pool.counts.max} · {isZh ? "类型待识别" : "Type pending"}: {pool.counts.unclassified}</p>
    <p className={pool.counts.available === 0 ? "font-medium text-red-600" : "text-slate-600"}>
      {accountStateLabel(pool.readiness, isZh)}
      {pool.counts.included === 0 && <span> · {isZh ? "当前无可用套餐账号" : "No included-quota account is currently available"}</span>}
    </p>
    </>}
    {windows.map(([name, window]) => {
      const used = window.used_percent;
      const complete = window.capacity_complete && used != null && window.remaining_percent != null && window.remaining_units != null;
      const pace = complete ? poolSteadyPacePercent(pool, name, now) : null;
      return <div key={name} className="space-y-1">
        <div className="flex flex-wrap items-center gap-2">
          <span className="min-w-14 font-medium text-slate-500">{name === "daily" ? (isZh ? "日额度" : "Daily") : (isZh ? "周额度" : "Weekly")}</span>
          {complete ? <>
            <UsageProgress usedPercent={used} pacePercent={pace} isZh={isZh} weighted
              label={name === "daily" ? (isZh ? "日额度已用" : "Daily used quota") : (isZh ? "周额度已用" : "Weekly used quota")} />
            <span className={`tabular-nums ${utilizationColor(used)}`}>{isZh ? "已用" : "Used"} {used.toFixed(1)}%</span>
            <span className="tabular-nums">{isZh ? "剩余" : "Remaining"} {window.remaining_percent!.toFixed(1)}%</span>
            <span className="text-slate-500">{window.remaining_units!.toFixed(2)} / {window.total_units} {isZh ? "份 Pro 等价额度" : "Pro-equivalent units"}</span>
          </> : <span className="text-slate-500">{!window.capacity_complete
            ? (isZh ? "账号类型待识别，容量尚未完整" : "Account types pending; capacity incomplete")
            : window.total_units === 0
              ? (isZh ? "无有效池容量" : "No eligible pool capacity")
              : (isZh ? "用量更新中" : "Updating usage")}</span>}
          {window.stale && <span className="text-amber-700">{isZh ? "旧数据 / 不完整" : "Stale / incomplete"}</span>}
        </div>
        {detailed && <>
        {pace != null && <p className="text-[10px] text-slate-500">
          {isZh ? `匀速使用预估：${pace.toFixed(1)}% · 按账号容量与各自重置时间加权` : `Steady-pace estimate: ${pace.toFixed(1)}% · Weighted by account capacity and individual reset times`}
        </p>}
        <p className="text-[10px] text-slate-500">{isZh ? "用量覆盖" : "Usage coverage"}: {window.known_accounts}/{window.total_accounts} {isZh ? "账号" : "accounts"}
          {name === "daily" && ` · ${isZh ? "仅非 Max 账号；Max 不计入日额度" : "Non-Max accounts only; Max has no daily quota"}`}
        </p>
        {window.next_reset_at && <p className="text-[10px] text-slate-500">
          {window.reset_mode === "aligned" ? (isZh ? "共同重置" : "Aligned reset") : (isZh ? "下次账号额度重置" : "Next account reset")}: <time dateTime={window.next_reset_at}>{new Date(window.next_reset_at).toLocaleString()}</time>
          {` (${window.next_reset_accounts}/${window.total_accounts} ${isZh ? "账号" : "accounts"})`}
          {window.reset_mode === "unknown" && ` · ${isZh ? "重置时间覆盖不完整" : "Incomplete reset coverage"}`}
        </p>}
        </>}
      </div>;
    })}
    {detailed && (pool.counts.disabled > 0 || pool.counts.disconnected > 0) && <p className="text-slate-500">
      {isZh ? "未计入容量" : "Excluded from capacity"}: {isZh ? "停用" : "disabled"} {pool.counts.disabled} · {isZh ? "断开" : "disconnected"} {pool.counts.disconnected}
    </p>}
  </div>;
}

/** Shared by the row footer and the group header so both read one cache entry. */
const isArkProvider = (provider: Provider) => provider.base_url.toLowerCase().includes("volces.com");

function arkCredentialsQuery(provider: Provider) {
  return {
    queryKey: ["provider-usage-credentials", provider.id],
    queryFn: () =>
      backend<ProviderUsageCredentials | null>("get_provider_usage_credentials", {
        id: provider.id,
      }).catch(() => null),
    staleTime: 5 * 60 * 1000,
    retry: false,
    refetchOnWindowFocus: false,
  };
}

function providerUsageQuery(provider: Provider) {
  return {
    queryKey: ["provider-usage", provider.id],
    queryFn: () => backend<ProviderUsage>("get_provider_usage", { id: provider.id }),
    // cc-switch polls the current provider every 5 minutes.
    refetchInterval: 5 * 60 * 1000,
    staleTime: 60 * 1000,
    retry: 1,
    refetchOnWindowFocus: false,
  };
}

const arkKeysConfigured = (credentials?: ProviderUsageCredentials | null) =>
  Boolean(credentials?.access_key?.trim() && credentials?.secret_key?.trim());

export function ProviderUsageFooter({ provider }: { provider: Provider }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const [now, setNow] = useState(() => Date.now());

  // Ark usage needs Volcengine IAM AK/SK (distinct from the inference key).
  // Until they are configured the footer stays hidden — no error banner.
  const isArk = isArkProvider(provider);
  const { data: arkCredentials } = useQuery<ProviderUsageCredentials | null>({
    ...arkCredentialsQuery(provider),
    enabled: isArk,
  });
  const arkConfigured = arkKeysConfigured(arkCredentials);

  const { data: usage, isFetching, error, refetch } = useQuery<ProviderUsage, Error>({
    ...providerUsageQuery(provider),
    enabled: !isArk || arkConfigured,
  });

  // Keep the "queried X ago" label and countdowns fresh.
  useEffect(() => {
    const interval = setInterval(() => setNow(Date.now()), 30_000);
    return () => clearInterval(interval);
  }, []);

  // Volcengine without IAM keys: render nothing (no warning text).
  if (isArk && !arkConfigured) {
    return null;
  }

  const shown = usage;
  const balances = shown?.balances ?? [];
  const quotaPaused = shown?.scheduling?.status === "quota_exhausted";
  const quotaCountdown = countdownStr(
    shown?.scheduling?.reset_at ?? shown?.scheduling?.next_check_at,
  );
  const blockedTierLabels = (shown?.scheduling?.blocking_tiers ?? [])
    .map((name) => tierLabel(name, isZh))
    .join(", ");
  // Devin reports an extra-usage balance next to its included windows: it is
  // what keeps the account usable once a window is spent.
  const isDevin = shown?.kind === "devin_plan";
  const extraBalanceLabel = isDevin ? (isZh ? "超额余额" : "Extra balance") : undefined;
  // Included quota spent, yet traffic still flows on the extra balance: the
  // bars read 100% but the provider is NOT paused, so say so explicitly.
  const onExtraBalance =
    isDevin &&
    !quotaPaused &&
    (shown?.tiers ?? []).some((tier) => tier.used_percent >= 100) &&
    balances.some((balance) => balance.total > 0);
  const title = providerUsageTitle(shown?.kind, isZh);
  const errorMessage = error ? localizeBackendErrorMessage(error, isZh) : "";

  const queriedAt = shown?.queried_at;
  const queriedAtMs = queriedAt ? Date.parse(queriedAt) : NaN;
  const hasTimestamp = Number.isFinite(queriedAtMs);
  const stale = Boolean(shown && (error || !hasTimestamp || now - queriedAtMs >= 5 * 60 * 1000));

  return (
    <div className="mt-3 space-y-2 rounded-xl border border-slate-200/70 bg-white/40 px-3 py-2.5">
      <div className="flex items-center justify-between gap-2">
        <div className="flex items-center gap-2 text-xs font-medium text-slate-500">
          <span>{title}</span>
          {shown?.level && (
            <span className="rounded bg-slate-100 px-1.5 py-0.5 text-[10px] font-semibold uppercase text-slate-600">
              {shown.level}
            </span>
          )}
        </div>
        <div className="flex items-center gap-2">
          {shown && (
            <span className={`flex items-center gap-1 text-[10px] ${stale ? "text-amber-700" : "text-slate-400"}`} title={queriedAt}>
              <Clock className="h-3 w-3" />
              {stale ? (isZh ? "旧数据" : "Stale") : (isZh ? "更新于" : "Updated")} {" "}
              {hasTimestamp ? <time dateTime={queriedAt}>{formatRelativeTime(queriedAt!, now, isZh)}</time> : (isZh ? "时间未知" : "time unknown")}
            </span>
          )}
          <button
            onClick={(e) => {
              e.stopPropagation();
              refetch();
            }}
            disabled={isFetching}
            title={isZh ? "刷新用量" : "Refresh usage"}
            className="cursor-pointer rounded p-1 text-slate-400 transition-colors hover:bg-slate-100 hover:text-slate-600 disabled:opacity-50"
          >
            <RefreshCw className={`h-3 w-3 ${isFetching ? "animate-spin" : ""}`} />
          </button>
        </div>
      </div>

      {quotaPaused && (
        <div className="flex items-center gap-2 border-y border-red-200 bg-red-50 px-2 py-1.5 text-xs text-red-700">
          <CirclePause className="h-3.5 w-3.5 shrink-0" />
          <span className="min-w-0 flex-1">
            {isZh ? "额度已满，已暂停调度" : "Quota exhausted, routing paused"}
            {blockedTierLabels && ` · ${blockedTierLabels}`}
          </span>
          {quotaCountdown && (
            <span className="shrink-0 font-medium tabular-nums">{quotaCountdown}</span>
          )}
        </div>
      )}

      {onExtraBalance && (
        <div className="flex items-center gap-2 border-y border-amber-200 bg-amber-50 px-2 py-1.5 text-xs text-amber-700">
          <AlertCircle className="h-3.5 w-3.5 shrink-0" />
          <span className="min-w-0 flex-1">
            {isZh
              ? "套餐额度已用完，正按超额余额计费，调度已降为最后备选"
              : "Included quota is spent; traffic now draws on the extra balance and is ranked last"}
          </span>
        </div>
      )}

      {isKiroOverage(shown) && (
        <div className="flex items-center gap-2 border-y border-amber-200 bg-amber-50 px-2 py-1.5 text-xs text-amber-700">
          <AlertCircle className="h-3.5 w-3.5 shrink-0" />
          <span className="min-w-0 flex-1">
            {isZh
              ? "套餐额度已用完，上游已启用超额计费，调度已降为最后备选。Nyro 不会主动开启付费功能。"
              : "Included quota is spent; upstream-enabled overage is billed and routing is ranked last. Nyro does not enable paid features."}
          </span>
        </div>
      )}

      {shown?.is_available === false && !quotaPaused && <p className="text-xs font-medium text-red-600">{isZh ? "不可用" : "Unavailable"}</p>}
      {error && <div role="alert" className="flex items-center gap-1.5 text-xs text-red-500" title={errorMessage}>
        <AlertCircle className="h-3.5 w-3.5 shrink-0" />
        <span className="break-words">{isZh ? "用量查询失败" : "Usage query failed"}: {errorMessage}</span>
      </div>}
      {shown && (shown.tiers.length > 0 || balances.length > 0 || (shown.spends?.length ?? 0) > 0) ? (
        <div className="flex flex-col gap-1.5">
          {/* Tier order is owned by the gateway API (canonical window order:
              5h → daily → weekly → monthly → …), so every consumer shows the
              same sequence. Do not re-sort here. */}
          {shown.tiers.map((tier) => (
            <TierBar key={tier.name} tier={tier} isZh={isZh} now={now} />
          ))}
          {/* Windows and balances render together: Devin's extra balance sits
              under its daily/weekly bars and decides whether a spent window
              pauses routing at all. */}
          {balances.map((balance) => (
            <BalanceRow
              key={balance.currency}
              balance={balance}
              isAvailable={shown?.is_available !== false}
              isZh={isZh}
              label={extraBalanceLabel}
            />
          ))}
          {(shown?.spends ?? []).map((spend) => (
            <SpendRow key={spend.name} spend={spend} isZh={isZh} />
          ))}
        </div>
      ) : isFetching ? (
        <p className="text-xs text-slate-400">{isZh ? "查询用量中..." : "Querying usage..."}</p>
      ) : (
        <p className="text-xs text-slate-400">{isZh ? "暂无用量数据" : "No usage data"}</p>
      )}
    </div>
  );

}

/**
 * Collapsed-group header summary: worst main-window usage across the members,
 * plus paused / unavailable / failed counts. Mounted only while the group is
 * folded; it shares query keys with each row's footer, so expanding reuses the
 * cached data instead of refetching.
 */
export function GroupUsageSummary({ providers }: { providers: Provider[] }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  // Devin shows its own account-pool card; it has no per-provider usage query.
  const members = providers.filter((p) => !isDevinProvider(p) && providerUsageSupported(p));

  const credentials = useQueries({
    queries: members.map((p) => ({ ...arkCredentialsQuery(p), enabled: isArkProvider(p) })),
  });
  // Ark without IAM keys renders nothing in its row either: leave it untracked.
  const tracked = members.filter((p, i) => !isArkProvider(p) || arkKeysConfigured(credentials[i]?.data));
  const queries = useQueries({
    queries: tracked.map((p) => providerUsageQuery(p)),
  });

  if (tracked.length === 0) return null;
  const summary = summarizeGroupUsage(
    tracked.map((p, i) => ({
      name: p.name,
      usage: queries[i]?.data,
      failed: Boolean(queries[i]?.error),
      loading: !queries[i]?.data && queries[i]?.isFetching,
    })),
  );
  const money = (n: number) => (Number.isFinite(n) ? n.toFixed(2) : "?");
  const detail = summary.perProvider
    .map((item) => `${item.name}: ${Math.round(item.percent)}%`)
    .join("\n");

  return (
    <span className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 text-xs text-slate-600" title={detail || undefined}>
      {summary.peak ? (
        <span className="inline-flex items-center gap-1.5">
          <span>{isZh ? "最高已用" : "Peak used"}</span>
          <span className={`font-semibold tabular-nums ${utilizationColor(summary.peak.percent)}`}>{Math.round(summary.peak.percent)}%</span>
          <span aria-hidden="true" className="h-1 w-10 overflow-hidden rounded-full bg-slate-200">
            <span className={`block h-full rounded-full ${utilizationBarClass(summary.peak.percent)}`} style={{ width: `${summary.peak.percent}%` }} />
          </span>
          {providers.length > 1 && <span className="max-w-32 truncate text-slate-400">{summary.peak.name}</span>}
        </span>
      ) : summary.balances.length > 0 ? (
        <span className="tabular-nums">
          {isZh ? "余额合计" : "Total balance"}: {summary.balances.map((b) => `${money(b.total)} ${b.currency}`).join(" / ")}
        </span>
      ) : summary.loading > 0 ? (
        <span className="text-slate-400">{isZh ? "查询用量中..." : "Querying usage..."}</span>
      ) : null}
      {summary.paused > 0 && (
        <span className="inline-flex items-center gap-1 font-medium text-red-600">
          <CirclePause aria-hidden="true" className="h-3 w-3" />
          {isZh ? `${summary.paused} 个已暂停` : `${summary.paused} paused`}
        </span>
      )}
      {summary.unavailable > 0 && <span className="font-medium text-red-600">{isZh ? `${summary.unavailable} 个不可用` : `${summary.unavailable} unavailable`}</span>}
      {summary.failed > 0 && (
        <span className="inline-flex items-center gap-1 text-amber-700">
          <AlertCircle aria-hidden="true" className="h-3 w-3" />
          {isZh ? `${summary.failed} 个查询失败` : `${summary.failed} query failed`}
        </span>
      )}
    </span>
  );
}
