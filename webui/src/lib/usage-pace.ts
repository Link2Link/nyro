import type { AccountPoolView } from "./devin-account-pool";
import type { ProviderUsageTier } from "./types";

/** Utilization → text color, mirroring cc-switch utilizationColor(). */
export function utilizationColor(utilization: number): string {
  if (utilization >= 90) return "text-red-500";
  if (utilization >= 70) return "text-orange-500";
  return "text-emerald-600";
}

/** Utilization → progress-bar fill color. */
export function utilizationBarClass(utilization: number): string {
  if (utilization >= 90) return "bg-red-500";
  if (utilization >= 70) return "bg-orange-500";
  return "bg-emerald-500";
}

/** Compact countdown like "2h30m" / "3d12h" (cc-switch countdownStr). */
export function countdownStr(resetsAt?: string | null, now = Date.now()): string | null {
  if (!resetsAt) return null;
  const diffMs = new Date(resetsAt).getTime() - now;
  if (!Number.isFinite(diffMs) || diffMs <= 0) return null;

  const hours = Math.floor(diffMs / (1000 * 60 * 60));
  const minutes = Math.floor((diffMs % (1000 * 60 * 60)) / (1000 * 60));

  if (hours > 24) {
    const days = Math.floor(hours / 24);
    return `${days}d${hours % 24}h`;
  }
  if (hours > 0) return `${hours}h${minutes}m`;
  return `${minutes}m`;
}

/** Window length per tier, used only for the visual steady-pace marker. */
export function tierWindowMs(name: string): number | null {
  // Feature-specific limits are not the provider's main routing quota, even
  // when their suffix names a known duration.
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
 * Visual time projection of even consumption, not an observed usage value.
 * Walk one window backwards from the reported reset: 0 at its start, 100 at
 * reset. An expired snapshot stays at 100; time alone never revives quota.
 */
export function steadyPacePercent(
  tier: Pick<ProviderUsageTier, "name" | "resets_at">,
  now: number,
): number | null {
  if (!tier.resets_at || !Number.isFinite(now)) return null;
  const windowMs = tierWindowMs(tier.name);
  const resetMs = Date.parse(tier.resets_at);
  if (windowMs == null || !Number.isFinite(resetMs)) return null;
  const remaining = resetMs - now;
  if (remaining <= 0) return 100;
  if (remaining >= windowMs) return 0;
  return ((windowMs - remaining) / windowMs) * 100;
}

/**
 * Capacity-weighted visual time projection across individual account resets.
 * These display-only weights match the backend's Pro-equivalent capacity:
 * daily Pro=1 (Max has no daily quota); weekly Pro=1, Max=10. Backend totals
 * and actual usage percentages remain authoritative and are never recomputed.
 * If membership, capacity, usage coverage, or reset coverage is incomplete,
 * hide the reference instead of guessing a plan or a common pool reset.
 */
export function poolSteadyPacePercent(
  pool: AccountPoolView,
  name: "daily" | "weekly",
  now: number,
): number | null {
  const window = pool.quota[name];
  if (
    !Number.isFinite(now) || !window || !window.capacity_complete
    || !Number.isFinite(window.total_units) || window.total_units <= 0
  ) return null;

  // Exhausted and auth-invalid accounts still belong to the backend capacity
  // denominator. Only disabled/disconnected accounts and verified Max daily
  // accounts are excluded; an unknown plan must never be guessed or skipped.
  const members = pool.accounts.filter((account) =>
    account.is_enabled && account.status !== "disconnected"
    && (name !== "daily" || account.verified_plan !== "max"),
  );
  if (
    window.total_accounts !== members.length
    || window.known_accounts !== window.total_accounts
    || [
      window.known_remaining_units, window.used_units, window.remaining_units,
      window.used_percent, window.remaining_percent,
    ].some((value) => value == null || !Number.isFinite(value))
  ) return null;

  let totalWeight = 0;
  let weightedPace = 0;
  for (const account of members) {
    if (account.verified_plan !== "pro" && account.verified_plan !== "max") return null;
    const quota = account[name];
    if (!quota || !Number.isFinite(quota.remaining_percent)) return null;
    const pace = steadyPacePercent({
      name: name === "daily" ? "daily" : "weekly_limit",
      resets_at: quota.resets_at,
    }, now);
    if (pace == null) return null;
    const weight = account.verified_plan === "max" ? 10 : 1;
    totalWeight += weight;
    weightedPace += weight * pace;
  }

  // Fail closed if the backend capacity rules or the account snapshot differ.
  if (totalWeight !== window.total_units) return null;
  return Math.min(100, Math.max(0, weightedPace / totalWeight));
}

/**
 * Display-only Claude pace, weighted by each account's own window reset.
 * Pro/unknown=1; Max 5x=5; Max 20x=20 for five-hour and 10 for weekly.
 * These estimates mirror backend capacity, never usage or routing decisions.
 * Incomplete/stale observations hide the marker rather than using the pool's
 * next_reset_at (which only describes the earliest account reset).
 */
export function claudePoolSteadyPacePercent(
  pool: AccountPoolView,
  name: string,
  now: number,
): number | null {
  if (!Number.isFinite(now) || (name !== "five_hour" && name !== "weekly_limit")) return null;
  const windows = pool.quota.claude_windows?.filter((window) => window.name === name);
  if (windows?.length !== 1) return null;
  const window = windows[0].quota;
  if (
    !window || !window.capacity_complete || window.stale
    || !Number.isFinite(window.total_units) || window.total_units <= 0
  ) return null;

  // Exhausted members still contribute capacity; only disabled/disconnected
  // accounts are excluded, matching the backend's accounting membership.
  const members = pool.accounts.filter((account) =>
    account.is_enabled && account.status !== "disconnected",
  );
  if (
    window.total_accounts !== members.length
    || window.known_accounts !== window.total_accounts
    || [window.known_remaining_units, window.used_units, window.remaining_units]
      .some((value) => value == null || !Number.isFinite(value) || value < 0 || value > window.total_units)
    || [window.used_percent, window.remaining_percent]
      .some((value) => value == null || !Number.isFinite(value) || value < 0 || value > 100)
  ) return null;

  let totalWeight = 0;
  let weightedPace = 0;
  for (const account of members) {
    if (account.stale || account.scheduling === "auth_required") return null;
    const usage = account.claude_usage;
    const quotas = usage?.windows?.filter((quota) => quota.name === name);
    if (quotas?.length !== 1) return null;
    const quota = quotas[0];
    if (!Number.isFinite(quota.used_percent) || quota.used_percent < 0 || quota.used_percent > 100) return null;
    const pace = steadyPacePercent({ name, resets_at: quota.resets_at }, now);
    if (pace == null) return null;
    const weight = usage?.plan === "max 5x" ? 5
      : usage?.plan === "max 20x" ? (name === "five_hour" ? 20 : 10) : 1;
    totalWeight += weight;
    weightedPace += weight * pace;
  }

  if (totalWeight !== window.total_units) return null;
  return Math.min(100, Math.max(0, weightedPace / totalWeight));
}
