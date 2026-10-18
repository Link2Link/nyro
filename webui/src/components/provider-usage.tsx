import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Clock, RefreshCw, AlertCircle, CirclePause } from "lucide-react";
import { backend } from "@/lib/backend";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { isKiroOverage, providerUsageTitle, tierLabel } from "@/lib/provider-usage";
import { useLocale } from "@/lib/i18n";
import { accountStateLabel, poolQuotaWindows, type AccountPoolView } from "@/lib/devin-account-pool";
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

/** Utilization → text color, mirroring cc-switch utilizationColor(). */
function utilizationColor(utilization: number): string {
  if (utilization >= 90) return "text-red-500";
  if (utilization >= 70) return "text-orange-500";
  return "text-emerald-600";
}

/** Utilization → progress-bar fill color. */
function utilizationBarClass(utilization: number): string {
  if (utilization >= 90) return "bg-red-500";
  if (utilization >= 70) return "bg-orange-500";
  return "bg-emerald-500";
}

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

/** Window length per tier, used to place the steady-pace marker. */
function tierWindowMs(name: string): number | null {
  // Feature-specific Codex limits are displayed but are not equivalent to the
  // provider's main routing quota. Do not show a misleading steady-pace marker
  // for them even when the suffix names a known duration.
  if (name.startsWith("feature:")) return null;
  switch (name) {
    case "five_hour":
      return 5 * 3600_000;
    case "daily":
      return 24 * 3600_000;
    case "weekly_limit":
      return 7 * 24 * 3600_000;
    case "monthly":
      return 30 * 24 * 3600_000;
    default:
      return null;
  }
}

/**
 * Steady-pace progress: the position (0-100) a perfectly even consumer would
 * sit at right now. Computed from the reset time walking the window
 * backwards: `elapsed / window` — 0 right after reset, 100 at reset time.
 * Returns null when the tier carries no reset time or unknown window.
 */
function steadyPacePercent(tier: ProviderUsageTier, now: number): number | null {
  if (!tier.resets_at) return null;
  const windowMs = tierWindowMs(tier.name);
  if (windowMs == null) return null;
  const remaining = new Date(tier.resets_at).getTime() - now;
  if (remaining <= 0) return 100;
  if (remaining >= windowMs) return 0;
  return ((windowMs - remaining) / windowMs) * 100;
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
      <div className="relative h-2 flex-1 overflow-hidden rounded-full bg-slate-100">
        <div
          className={`h-full rounded-full transition-all ${utilizationBarClass(used)}`}
          style={{ width: `${used}%` }}
        />
        {pace != null && (
          <div
            // Steady-pace marker: where an even consumer would be right now
            // (0% right after reset → 100% at reset time). White-outlined
            // needle stays legible over both the colored fill and the pale
            // unused track.
            className="absolute inset-y-0 w-[3px] -translate-x-1/2 rounded-full bg-slate-700 shadow-[0_0_0_1.5px_rgba(255,255,255,0.9)]"
            style={{ left: `${Math.min(Math.max(pace, 0), 100)}%` }}
            title={
              isZh
                ? `匀速参考线：${Math.round(pace)}%（按整窗匀速使用推进）`
                : `Steady pace: ${Math.round(pace)}% (even consumption over the window)`
            }
          />
        )}
      </div>
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

/** Backend-owned pool percentages; deliberately no synthetic common reset/pace marker. */
export function DevinPoolUsage({ pool }: { pool: AccountPoolView }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const windows = poolQuotaWindows(pool);
  if (!pool.config) return null;
  return <div className="space-y-2 rounded-lg border border-slate-200/70 bg-white/40 px-3 py-2 text-xs">
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
      <strong>Devin · {pool.config.plan === "pro" ? "Pro" : "Max"}</strong>
      <span>{isZh ? `${pool.counts.total} 个账号 · ${pool.counts.available} 个可用` : `${pool.counts.total} accounts · ${pool.counts.available} available`}</span>
      {pool.quota.estimated && <span className="text-amber-700">{isZh ? "容量估算" : "Estimated capacity"}</span>}
    </div>
    <p className={pool.counts.available === 0 ? "font-medium text-red-600" : "text-slate-600"}>
      {accountStateLabel(pool.readiness, isZh)}
      {pool.counts.included === 0 && <span> · {isZh ? "当前无可用套餐账号" : "No included-quota account is currently available"}</span>}
    </p>
    {windows.map(([name, window]) => {
      const used = window.used_percent;
      const complete = used != null && window.remaining_percent != null && window.remaining_units != null;
      return <div key={name} className="space-y-1">
        <div className="flex flex-wrap items-center gap-2">
          <span className="min-w-14 font-medium text-slate-500">{name === "daily" ? (isZh ? "日额度" : "Daily") : (isZh ? "周额度" : "Weekly")}</span>
          {complete ? <>
            <div role="progressbar" aria-label={name === "daily" ? (isZh ? "日额度已用" : "Daily used quota") : (isZh ? "周额度已用" : "Weekly used quota")}
              aria-valuemin={0} aria-valuemax={100} aria-valuenow={used}
              className="h-2 min-w-20 flex-1 overflow-hidden rounded-full bg-slate-100">
              <div className={`h-full rounded-full ${utilizationBarClass(used)}`} style={{ width: `${Math.min(100, Math.max(0, used))}%` }} />
            </div>
            <span className={`tabular-nums ${utilizationColor(used)}`}>{isZh ? "已用" : "Used"} {used.toFixed(1)}%</span>
            <span className="tabular-nums">{isZh ? "剩余" : "Remaining"} {window.remaining_percent!.toFixed(1)}%</span>
            <span className="text-slate-500">{window.remaining_units!.toFixed(2)} / {window.total_units} {isZh ? "份账号额度" : "account units"}</span>
          </> : <span className="text-slate-500">{window.total_units === 0
            ? (isZh ? "无有效池容量" : "No eligible pool capacity")
            : (isZh ? `更新中（${window.known_accounts}/${window.total_units} 账号）` : `Updating (${window.known_accounts}/${window.total_units} accounts)`)}</span>}
          {window.stale && <span className="text-amber-700">{isZh ? "旧数据 / 不完整" : "Stale / incomplete"}</span>}
        </div>
        {window.next_reset_at && <p className="text-[10px] text-slate-500">
          {window.reset_mode === "aligned" ? (isZh ? "共同重置" : "Aligned reset") : (isZh ? "下次账号额度重置" : "Next account reset")}: <time dateTime={window.next_reset_at}>{new Date(window.next_reset_at).toLocaleString()}</time>
          {` (${window.next_reset_accounts}/${window.total_units} ${isZh ? "账号" : "accounts"})`}
          {window.reset_mode === "unknown" && ` · ${isZh ? "重置时间覆盖不完整" : "Incomplete reset coverage"}`}
        </p>}
      </div>;
    })}
    {(pool.counts.disabled > 0 || pool.counts.disconnected > 0 || pool.counts.plan_mismatch > 0 || pool.counts.plan_unverified > 0) && <p className="text-slate-500">
      {isZh ? "容量排除" : "Capacity exclusions"}: {isZh ? "停用" : "disabled"} {pool.counts.disabled} · {isZh ? "断开" : "disconnected"} {pool.counts.disconnected} · {isZh ? "套餐不匹配" : "plan mismatch"} {pool.counts.plan_mismatch} · {isZh ? "未验证" : "unverified"} {pool.counts.plan_unverified}
    </p>}
  </div>;
}

export function ProviderUsageFooter({
  provider,
  compact = false,
}: {
  provider: Provider;
  compact?: boolean;
}) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const [now, setNow] = useState(() => Date.now());

  // Ark usage needs Volcengine IAM AK/SK (distinct from the inference key).
  // Until they are configured the footer stays hidden — no error banner.
  const isArk = provider.base_url.toLowerCase().includes("volces.com");
  const { data: arkCredentials } = useQuery<ProviderUsageCredentials | null>({
    queryKey: ["provider-usage-credentials", provider.id],
    queryFn: () =>
      backend<ProviderUsageCredentials | null>("get_provider_usage_credentials", {
        id: provider.id,
      }).catch(() => null),
    enabled: isArk,
    staleTime: 5 * 60 * 1000,
    retry: false,
    refetchOnWindowFocus: false,
  });
  const arkConfigured = Boolean(
    arkCredentials?.access_key?.trim() && arkCredentials?.secret_key?.trim(),
  );

  const { data: usage, isFetching, error, refetch } = useQuery<ProviderUsage, Error>({
    queryKey: ["provider-usage", provider.id],
    queryFn: () => backend<ProviderUsage>("get_provider_usage", { id: provider.id }),
    // cc-switch polls the current provider every 5 minutes.
    refetchInterval: 5 * 60 * 1000,
    staleTime: 60 * 1000,
    retry: 1,
    refetchOnWindowFocus: false,
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

  const fullFooter = (
    <div className={compact ? "mt-2 space-y-2 border-t border-slate-200/70 pt-2" : "mt-3 space-y-2 rounded-xl border border-slate-200/70 bg-white/40 px-3 py-2.5"}>
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
          {shown?.queried_at && (
            <span className="flex items-center gap-1 text-[10px] text-slate-400">
              <Clock className="h-3 w-3" />
              {formatRelativeTime(shown.queried_at, now, isZh)}
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

      {error && !shown ? (
        <div className="flex items-center gap-1.5 text-xs text-red-500" title={errorMessage}>
          <AlertCircle className="h-3.5 w-3.5 shrink-0" />
          <span className="truncate">{errorMessage}</span>
        </div>
      ) : shown && (shown.tiers.length > 0 || balances.length > 0) ? (
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

  // Keep the default footer and its fetching lifecycle unchanged. Native
  // details provides keyboard disclosure without mounting another query.
  if (!compact) return fullFooter;

  const tiers = shown?.tiers ?? [];
  const spends = shown?.spends ?? [];
  const queriedAt = shown?.queried_at;
  const queriedAtMs = queriedAt ? Date.parse(queriedAt) : NaN;
  const hasTimestamp = Number.isFinite(queriedAtMs);
  // Treat a missed polling interval or a failed refresh as stale; never
  // present cached values as a successful live query after an error.
  const stale = Boolean(shown && (error || !hasTimestamp || now - queriedAtMs >= 5 * 60 * 1000));
  const unavailable = isZh ? "未知" : "Unknown";
  const formatAmount = (amount: number, currency: string) =>
    Number.isFinite(amount) ? `${amount.toFixed(2)} ${currency}` : unavailable;
  const extraCount = Math.max((tiers.length || balances.length || spends.length) - 2, 0);

  return (
    <details className="group mt-2 rounded-lg border border-slate-200/70 bg-white/40 px-2.5 py-1.5">
      <summary
        className="flex cursor-pointer list-none flex-wrap items-center gap-x-3 gap-y-1 rounded text-xs text-slate-600 outline-none focus-visible:ring-2 focus-visible:ring-slate-400 [&::-webkit-details-marker]:hidden"
        onClick={(event) => event.stopPropagation()}
      >
        <span className="font-medium">{title}</span>
        {tiers.length > 0 ? tiers.slice(0, 2).map((tier) => {
          const used = Number.isFinite(tier.used_percent)
            ? Math.min(Math.max(tier.used_percent, 0), 100)
            : null;
          return (
            <span key={tier.name} className="inline-flex flex-wrap items-center gap-1.5">
              <span>{tierLabel(tier.name, isZh)}</span>
              <span className={`font-semibold tabular-nums ${used == null ? "text-slate-400" : utilizationColor(used)}`}>
                {isZh ? "已用" : "Used"} {used == null ? unavailable : `${Math.round(used)}%`}
              </span>
              {used != null && (
                <span aria-hidden="true" className="h-1 w-10 overflow-hidden rounded-full bg-slate-200">
                  <span className={`block h-full rounded-full ${utilizationBarClass(used)}`} style={{ width: `${used}%` }} />
                </span>
              )}
            </span>
          );
        }) : balances.length > 0 ? balances.slice(0, 2).map((balance) => (
          <span key={balance.currency} className="tabular-nums">
            {extraBalanceLabel ?? (isZh ? "余额" : "Balance")}: {formatAmount(balance.total, balance.currency)}
          </span>
        )) : spends.length > 0 ? spends.slice(0, 2).map((spend) => (
          <span key={spend.name} className="tabular-nums">
            {spend.name === "today" ? (isZh ? "今日" : "Today") : spend.name === "month" ? (isZh ? "本月" : "Month") : spend.name}{" "}
            {isZh ? "已花费" : "Spent"}: {formatAmount(spend.amount, spend.currency)}
          </span>
        )) : (
          <span className="text-slate-400">
            {isFetching ? (isZh ? "查询用量中..." : "Querying usage...") : (isZh ? "暂无用量数据" : "No usage data")}
          </span>
        )}
        {extraCount > 0 && <span>{isZh ? `另有 ${extraCount} 项` : `+${extraCount} more`}</span>}
        {shown?.is_available === false && (
          <span className="font-medium text-red-600">{isZh ? "不可用" : "Unavailable"}</span>
        )}
        {quotaPaused && (
          <span className="font-medium text-red-600">
            {isZh ? "额度已满，已暂停调度" : "Quota exhausted, routing paused"}
            {blockedTierLabels && ` · ${blockedTierLabels}`}
            {quotaCountdown && ` · ${quotaCountdown}`}
          </span>
        )}
        {onExtraBalance && (
          <span className="text-amber-700">
            {isZh ? "套餐额度已用完，正按超额余额计费，调度已降为最后备选" : "Included quota spent; using extra balance, routing ranked last"}
          </span>
        )}
        {isKiroOverage(shown) && (
          <span className="text-amber-700">
            {isZh ? "上游已启用超额计费，调度已降为最后备选。Nyro 不会主动开启付费功能。" : "Upstream-enabled overage billed; routing ranked last. Nyro does not enable paid features."}
          </span>
        )}
        {error && (
          <span className="inline-flex min-w-0 items-center gap-1 break-words text-red-600">
            <AlertCircle aria-hidden="true" className="h-3 w-3 shrink-0" />
            {isZh ? "用量查询失败" : "Usage query failed"}: {errorMessage}
          </span>
        )}
        {shown && (
          <span className={stale ? "text-amber-700" : "text-slate-400"} title={queriedAt}>
            {stale ? (isZh ? "旧数据" : "Stale") : (isZh ? "更新于" : "Updated")}{" "}
            {hasTimestamp ? (
              <time dateTime={queriedAt}>{formatRelativeTime(queriedAt!, now, isZh)}</time>
            ) : (isZh ? "时间未知" : "time unknown")}
          </span>
        )}
        <span className="ml-auto whitespace-nowrap font-medium text-slate-500">
          <span className="group-open:hidden">{isZh ? "展开详情" : "Show details"} ▾</span>
          <span className="hidden group-open:inline">{isZh ? "收起详情" : "Hide details"} ▴</span>
        </span>
      </summary>
      {fullFooter}
    </details>
  );
}
