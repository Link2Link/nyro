import type { AccountPoolView, ProviderAccountView } from "./devin-account-pool";
import { claudeWindowView } from "./claude-usage-windows";
import { kiroAccountResources } from "./kiro-pool-usage";
import type { AccountUsageStats, EquivalentCostTotal } from "./types";

export type AccountUsageKind = NonNullable<AccountPoolView["kind"]>;

// Same rounding budget as CLAUDE_SAME_CYCLE_SKEW_MS in provider/account_pool.rs.
// Unlike backend display merging, projection must reject later-cycle headers too.
const CLAUDE_SAME_CYCLE_SKEW_MS = 120_000;
const WEEK_MS = 7 * 86_400_000;

function cycleUsedPercent(account: ProviderAccountView, kind: AccountUsageKind, end: number, now: number): number | null {
  const matchesReset = (reset: string | null | undefined) => reset != null && Date.parse(reset) === end;
  if (kind === "devin") {
    const weekly = account.weekly;
    return weekly && matchesReset(weekly.resets_at) && Number.isFinite(weekly.remaining_percent)
      && weekly.remaining_percent >= 0 && weekly.remaining_percent <= 100 ? 100 - weekly.remaining_percent : null;
  }
  if (kind === "claude_code") {
    // Never extrapolate weekly log prices from a 5h or model-specific window.
    const windows = account.claude_usage?.windows.filter((window) => window.name === "weekly_limit") ?? [];
    if (windows.length !== 1) return null;
    const window = windows[0];
    if (window.source !== "inference_header") return matchesReset(window.resets_at) ? window.used_percent : null;
    const reset = window.resets_at == null ? NaN : Date.parse(window.resets_at);
    // Only match active overlapping weeks; tolerance must never extend either cycle
    // or change the backend interval used to price retained logs. Exact header
    // matches still require their own fresh observation, not the full snapshot clock.
    return Number.isFinite(reset) && Math.abs(reset - end) <= CLAUDE_SAME_CYCLE_SKEW_MS
      && reset - WEEK_MS <= now && now < reset && claudeWindowView(window, account, now).fresh
      ? window.used_percent : null;
  }
  const usage = account.kiro_usage;
  if (!usage) return null;
  const base = usage.items.filter((item) => item.kind === "base");
  if (base.length ? !base.every((item) => matchesReset(item.resets_at ?? usage.resets_at)) : !matchesReset(usage.resets_at)) return null;
  // Do not silently drop invalid active allowances and extrapolate incomplete capacity.
  if (usage.items.some((item) => {
    if (item.kind !== "base" && item.resets_at != null && Date.parse(item.resets_at) <= now) return false;
    return !Number.isFinite(item.used) || item.used < 0 || !Number.isFinite(item.limit) || item.limit < 0;
  })) return null;
  // Reuse the displayed Kiro capacity: base + active grants, excluding overage.
  // Different resource units cannot be added or averaged into one account ratio.
  const resources = [...kiroAccountResources(usage, now).values()].filter((resource) => resource.limit > 0);
  return resources.length === 1 ? resources[0].used / resources[0].limit * 100 : null;
}

/** Display-only linear projection, not remaining balance, scheduling capacity or billing. */
export function estimateAccountUsagePrice(stats: AccountUsageStats, account: ProviderAccountView, kind: AccountUsageKind, now: number): {
  costs: EquivalentCostTotal[];
  usedPercent: number;
} | null {
  const { period, period_start: start, period_end: end } = stats;
  if (account.account_id !== stats.account_id || account.stale || !period || period.request_count <= 0
    || start == null || end == null || !(start < end && start <= now && now < end)
    || !Number.isFinite(new Date(start).getTime()) || !Number.isFinite(new Date(end).getTime())) return null;
  const usedPercent = cycleUsedPercent(account, kind, end, now);
  if (usedPercent == null || !Number.isFinite(usedPercent) || usedPercent <= 0 || usedPercent > 100) return null;
  const costs = period.equivalent_costs.filter((cost) => Number.isFinite(cost.total_cost) && cost.total_cost >= 0 && cost.priced_request_count > 0)
    .map((cost) => ({ ...cost, total_cost: cost.total_cost / (usedPercent / 100) }));
  if (!costs.length || costs.some((cost) => !Number.isFinite(cost.total_cost))) return null;
  return { costs, usedPercent };
}
