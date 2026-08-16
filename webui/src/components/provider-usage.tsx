import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Clock, RefreshCw, AlertCircle } from "lucide-react";
import { backend } from "@/lib/backend";
import { useLocale } from "@/lib/i18n";
import type {
  Provider,
  ProviderUsage,
  ProviderUsageBalance,
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

const TIER_LABELS_ZH: Record<string, string> = {
  five_hour: "5小时",
  weekly_limit: "每周",
  monthly: "每月",
};
const TIER_LABELS_EN: Record<string, string> = {
  five_hour: "5h",
  weekly_limit: "Weekly",
  monthly: "Monthly",
};

function tierLabel(name: string, isZh: boolean): string {
  const table = isZh ? TIER_LABELS_ZH : TIER_LABELS_EN;
  return table[name] ?? name;
}

function formatRelativeTime(timestamp: string, now: number, isZh: boolean): string {
  const diff = Math.floor((now - new Date(timestamp).getTime()) / 1000);
  if (diff < 60) return isZh ? "刚刚" : "just now";
  if (diff < 3600) return isZh ? `${Math.floor(diff / 60)} 分钟前` : `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return isZh ? `${Math.floor(diff / 3600)} 小时前` : `${Math.floor(diff / 3600)}h ago`;
  return isZh ? `${Math.floor(diff / 86400)} 天前` : `${Math.floor(diff / 86400)}d ago`;
}

function TierBar({ tier, isZh }: { tier: ProviderUsageTier; isZh: boolean }) {
  const countdown = countdownStr(tier.resets_at);
  const used = Math.min(Math.max(tier.used_percent, 0), 100);
  return (
    <div className="flex items-center gap-3 text-xs">
      <span className="w-14 shrink-0 font-medium text-slate-500">
        {tierLabel(tier.name, isZh)}
      </span>
      <div className="h-2 flex-1 overflow-hidden rounded-full bg-slate-100">
        <div
          className={`h-full rounded-full transition-all ${utilizationBarClass(used)}`}
          style={{ width: `${used}%` }}
        />
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

function BalanceRow({
  balance,
  isAvailable,
  isZh,
}: {
  balance: ProviderUsageBalance;
  isAvailable: boolean;
  isZh: boolean;
}) {
  const symbol = balance.currency === "CNY" ? "¥" : balance.currency === "USD" ? "$" : "";
  const fmt = (value: number) => value.toFixed(2);
  // cc-switch colors balance rows by availability; keep low-balance red at
  // the same 70/90 thresholds when a rough floor of 10 is assumed absent.
  const color = isAvailable ? "text-emerald-600" : "text-red-500";
  return (
    <div className="flex items-center justify-between gap-3 text-xs">
      <span className="w-14 shrink-0 font-medium text-slate-500">
        {isZh ? "余额" : "Balance"}
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

export function ProviderUsageFooter({ provider }: { provider: Provider }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const [now, setNow] = useState(() => Date.now());

  const { data: usage, isFetching, error, refetch } = useQuery<ProviderUsage, Error>({
    queryKey: ["provider-usage", provider.id],
    queryFn: () => backend<ProviderUsage>("get_provider_usage", { id: provider.id }),
    // cc-switch polls the current provider every 5 minutes.
    refetchInterval: 5 * 60 * 1000,
    staleTime: 60 * 1000,
    retry: 1,
    refetchOnWindowFocus: false,
  });

  // Keep the "queried X ago" label and countdowns fresh.
  useEffect(() => {
    const interval = setInterval(() => setNow(Date.now()), 30_000);
    return () => clearInterval(interval);
  }, []);

  const shown = usage;
  const balances = shown?.balances ?? [];
  const isBalanceView = shown?.kind.endsWith("_balance") ?? false;
  const title = isBalanceView
    ? isZh
      ? "账户余额"
      : "Balance"
    : isZh
      ? "套餐用量"
      : "Plan Usage";

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

      {error && !shown ? (
        <div className="flex items-center gap-1.5 text-xs text-red-500" title={error.message}>
          <AlertCircle className="h-3.5 w-3.5 shrink-0" />
          <span className="truncate">{error.message}</span>
        </div>
      ) : shown && shown.tiers.length > 0 ? (
        <div className="flex flex-col gap-1.5">
          {shown.tiers.map((tier) => (
            <TierBar key={tier.name} tier={tier} isZh={isZh} />
          ))}
        </div>
      ) : balances.length > 0 ? (
        <div className="flex flex-col gap-1.5">
          {balances.map((balance) => (
            <BalanceRow
              key={balance.currency}
              balance={balance}
              isAvailable={shown?.is_available !== false}
              isZh={isZh}
            />
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
