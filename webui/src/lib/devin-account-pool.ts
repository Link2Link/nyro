export type DevinPlan = "pro" | "max";

/** Mirrors the public nyro_core::admin contract. Never store credentials here. */
export interface DevinPoolConfig {
  /** Legacy input only; never use this to determine account types or capacity. */
  plan?: DevinPlan;
  strategy: "round_robin" | "fill_first";
  session_affinity: boolean;
  affinity_ttl_seconds: number;
  max_account_attempts: number;
  reserve_policy: "disabled" | "fallback";
}

export interface AccountQuotaWindow {
  remaining_percent: number;
  resets_at: string | null;
}

export interface ProviderAccountView {
  account_id: string;
  provider_id: string;
  label: string;
  is_enabled: boolean;
  priority: number;
  status: string;
  status_version: number;
  verified_plan: DevinPlan | null;
  identity_verified: boolean;
  scheduling: string;
  daily: AccountQuotaWindow | null;
  weekly: AccountQuotaWindow | null;
  reserve_usd: number | null;
  observed_at: number | null;
  stale: boolean;
  updated_at: string;
}

export interface PoolQuotaWindow {
  /** Capacity is weighted in Pro-equivalent units, not account count. */
  total_units: number;
  total_accounts: number;
  capacity_complete: boolean;
  known_accounts: number;
  known_remaining_units: number;
  used_units: number | null;
  remaining_units: number | null;
  used_percent: number | null;
  remaining_percent: number | null;
  stale: boolean;
  next_reset_at: string | null;
  next_reset_accounts: number;
  reset_mode: string;
}

export interface AccountPoolCounts {
  total: number; accounting: number; available: number; included: number; reserve: number;
  pro: number; max: number; unclassified: number;
  unknown: number; disabled: number; disconnected: number; auth_required: number;
  quota_exhausted: number; plan_mismatch: number; plan_unverified: number;
}

export interface AccountPoolView {
  provider_id: string;
  config: DevinPoolConfig | null;
  accounts: ProviderAccountView[];
  quota: { daily: PoolQuotaWindow | null; weekly: PoolQuotaWindow | null; estimated: boolean };
  counts: AccountPoolCounts;
  readiness: string;
}

export interface OAuthAccountUpdate {
  label?: string;
  is_enabled?: boolean;
  priority?: number;
}

export function accountStateLabel(state: string, isZh: boolean): string {
  const labels: Record<string, [string, string]> = {
    connected: ["Connected", "已授权"], disconnected: ["Disconnected", "已断开"],
    error: ["Authorization error", "授权错误"], pending: ["Pending", "等待中"],
    not_connected: ["Not connected", "未授权"], refreshing: ["Refreshing", "刷新中"],
    included: ["Included quota", "套餐可用"], eligible: ["Available", "可用"],
    reserve: ["Paid balance", "付费余额"], reserve_only: ["Paid balance only", "仅付费余额可用"],
    unknown: ["Usage unknown", "用量未知"], unknown_usage: ["Usage unknown", "用量未知"],
    disabled: ["Disabled", "已停用"], auth_required: ["Needs login", "需要登录"],
    quota_exhausted: ["Quota exhausted", "额度耗尽"], plan_mismatch: ["Plan mismatch", "套餐不匹配"],
    plan_unverified: ["Plan unverified", "套餐未验证"], no_account: ["No account", "无账号"],
    plan_verification_required: ["Plan verification required", "需要验证套餐"],
  };
  return labels[state]?.[isZh ? 1 : 0] ?? state;
}

/** Legacy login remains available without guessing Pro/Max; never choose among accounts. */
export function canReauthorizeLegacyProvider(pool: AccountPoolView, providerId: string): boolean {
  return pool.provider_id === providerId && pool.config === null && pool.accounts.length <= 1
    && pool.accounts.every((account) => account.provider_id === providerId);
}

/** Select windows only; aggregation and null/coverage semantics belong to core. */
export function poolQuotaWindows(pool: AccountPoolView): ["daily" | "weekly", PoolQuotaWindow][] {
  if (!pool.config) return [];
  const windows: ["daily" | "weekly", PoolQuotaWindow][] = [];
  if (pool.quota.daily) windows.push(["daily", pool.quota.daily]);
  if (pool.quota.weekly) windows.push(["weekly", pool.quota.weekly]);
  return windows;
}

/** Account types are detected upstream; configuration only controls scheduling. */
export function newDevinPoolConfig(): DevinPoolConfig {
  return {
    strategy: "round_robin",
    session_affinity: true,
    affinity_ttl_seconds: 3600,
    max_account_attempts: 3,
    reserve_policy: "disabled",
  };
}
