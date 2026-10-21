import type { AccountPoolView, KiroUsage, ProviderAccountView } from "./devin-account-pool";
import { steadyPacePercent } from "./usage-pace";

/** One Kiro resource (e.g. CREDIT) summed across every pool member that reports it. */
export interface KiroPoolResource {
  resource: string;
  /** Sum of each account's pooled `min(used, limit)`; overage is never counted as capacity. */
  used: number;
  limit: number;
  /** 0–100; an explicit zero total allowance reads as exhausted, like the singleton tier. */
  usedPercent: number;
  remainingPercent: number;
  /** Accounts whose snapshot contributed to this resource. */
  accounts: number;
  /** Contributing accounts whose snapshot is stale. */
  staleAccounts: number;
  /** Earliest base reset among contributing accounts. */
  nextResetAt: string | null;
  /** Contributing accounts that reset at `nextResetAt`. */
  nextResetAccounts: number;
  /** Every contributing account reports the same base reset. */
  resetAligned: boolean;
  /**
   * Limit-weighted monthly steady pace across individual account resets.
   * Null whenever any contributing account lacks a usable reset.
   */
  pacePercent: number | null;
}

export interface KiroPoolUsageSummary {
  /** Enabled, non-disconnected accounts: the same membership the backend schedules. */
  members: number;
  /** Members with at least one valid resource observation. */
  observed: number;
  resources: KiroPoolResource[];
}

interface AccountResource {
  used: number;
  limit: number;
  resetMs: number | null;
}

const validNumber = (value: number) => Number.isFinite(value) && value >= 0;

/**
 * Mirror of core `KiroUsage::resource_pools_at`: base + active bonus/trial
 * allowances of one resource are additive; expired grants contribute nothing,
 * and only base allowances own the resource's reset.
 */
function accountResources(usage: KiroUsage, now: number): Map<string, AccountResource> {
  const pools = new Map<string, AccountResource>();
  for (const item of usage.items) {
    if (!validNumber(item.used) || !validNumber(item.limit)) continue;
    let pool = pools.get(item.resource);
    if (!pool) {
      pool = { used: 0, limit: 0, resetMs: null };
      pools.set(item.resource, pool);
    }
    const itemReset = item.resets_at ? Date.parse(item.resets_at) : NaN;
    if (item.kind !== "base" && Number.isFinite(itemReset) && itemReset <= now) continue;
    pool.used += Math.min(item.used, item.limit);
    pool.limit += item.limit;
    if (item.kind === "base") {
      const raw = item.resets_at ?? usage.resets_at;
      const reset = raw ? Date.parse(raw) : NaN;
      if (Number.isFinite(reset)) pool.resetMs = pool.resetMs == null ? reset : Math.min(pool.resetMs, reset);
    }
  }
  return pools;
}

function poolMembers(pool: AccountPoolView): ProviderAccountView[] {
  return pool.accounts.filter((account) => account.is_enabled && account.status !== "disconnected");
}

/**
 * Display-only pool total for Kiro: each resource's actual used/limit summed
 * across accounts. Different resources are never merged with each other, and
 * scheduling stays per-account in core.
 */
export function kiroPoolUsage(pool: AccountPoolView, now: number): KiroPoolUsageSummary {
  const members = poolMembers(pool);
  const totals = new Map<string, {
    used: number; limit: number; accounts: number; staleAccounts: number;
    resets: (number | null)[]; weighted: { limit: number; resetMs: number | null }[];
  }>();
  let observed = 0;
  for (const account of members) {
    if (!account.kiro_usage) continue;
    const resources = accountResources(account.kiro_usage, now);
    if (resources.size === 0) continue;
    observed += 1;
    for (const [resource, value] of resources) {
      let total = totals.get(resource);
      if (!total) {
        total = { used: 0, limit: 0, accounts: 0, staleAccounts: 0, resets: [], weighted: [] };
        totals.set(resource, total);
      }
      total.used += value.used;
      total.limit += value.limit;
      total.accounts += 1;
      if (account.stale) total.staleAccounts += 1;
      total.resets.push(value.resetMs);
      total.weighted.push({ limit: value.limit, resetMs: value.resetMs });
    }
  }

  const resources = [...totals.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([resource, total]): KiroPoolResource => {
      const usedPercent = total.limit > 0 ? Math.min(100, Math.max(0, total.used / total.limit * 100)) : 100;
      const knownResets = total.resets.filter((reset): reset is number => reset != null);
      const nextResetMs = knownResets.length ? Math.min(...knownResets) : null;
      return {
        resource,
        used: total.used,
        limit: total.limit,
        usedPercent,
        remainingPercent: 100 - usedPercent,
        accounts: total.accounts,
        staleAccounts: total.staleAccounts,
        nextResetAt: nextResetMs == null ? null : new Date(nextResetMs).toISOString(),
        nextResetAccounts: nextResetMs == null ? 0 : knownResets.filter((reset) => reset === nextResetMs).length,
        resetAligned: knownResets.length === total.resets.length && knownResets.every((reset) => reset === nextResetMs),
        pacePercent: weightedMonthlyPace(total.weighted, now),
      };
    });
  return { members: members.length, observed, resources };
}

/** Each account's monthly window progress, weighted by its share of the summed allowance. */
function weightedMonthlyPace(parts: { limit: number; resetMs: number | null }[], now: number): number | null {
  let totalWeight = 0;
  let weighted = 0;
  for (const part of parts) {
    if (part.resetMs == null) return null;
    const pace = steadyPacePercent({ name: "monthly", resets_at: new Date(part.resetMs).toISOString() }, now);
    if (pace == null) return null;
    totalWeight += part.limit;
    weighted += part.limit * pace;
  }
  if (totalWeight <= 0) return null;
  return Math.min(100, Math.max(0, weighted / totalWeight));
}

/** Labels pasted from a credential file would be shown on every card; refuse token-shaped text. */
export function looksLikeCredential(label: string): boolean {
  const value = label.trim();
  return /\b(refresh|access)_?token\b|clientSecret|client_secret/i.test(value)
    || /\b(aor|aoa)A{4,}[A-Za-z0-9_-]{16,}/.test(value)
    || /\bksk_[A-Za-z0-9]{8,}/.test(value)
    || /\beyJ[A-Za-z0-9_-]{20,}/.test(value);
}
