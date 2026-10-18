import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { accountStateLabel, canReauthorizeLegacyProvider, newDevinPoolConfig, poolQuotaWindows, type AccountPoolView, type PoolQuotaWindow } from "./devin-account-pool";
import { localizeBackendErrorMessage } from "./backend-error";

const window: PoolQuotaWindow = {
  total_units: 3, total_accounts: 3, capacity_complete: true, known_accounts: 2, known_remaining_units: 1.5,
  used_units: null, remaining_units: null, used_percent: null, remaining_percent: null,
  stale: true, next_reset_at: "2030-01-01T01:00:00Z", next_reset_accounts: 1, reset_mode: "unknown",
};
const pool: AccountPoolView = {
  provider_id: "devin", config: newDevinPoolConfig(), accounts: [],
  quota: { daily: window, weekly: window, estimated: true },
  counts: { total: 3, pro: 3, max: 0, unclassified: 0, accounting: 3, available: 0, included: 0, reserve: 0, unknown: 0, disabled: 0, disconnected: 0, auth_required: 0, quota_exhausted: 3, plan_mismatch: 0, plan_unverified: 0 },
  readiness: "quota_exhausted",
};

test("Pro exposes backend daily and weekly snapshots without filling unknown totals", () => {
  const windows = poolQuotaWindows(pool);
  deepEqual(windows.map(([name]) => name), ["daily", "weekly"]);
  equal(windows[0][1], window);
  equal(windows[0][1].remaining_percent, null);
  equal(windows[0][1].total_units, 3);
  equal(windows[0][1].next_reset_accounts, 1);
});

test("backend-null daily windows stay hidden for Max, and legacy never invents a pool", () => {
  deepEqual(poolQuotaWindows({
    ...pool, counts: { ...pool.counts, pro: 0, max: 3 }, quota: { ...pool.quota, daily: null },
  }).map(([name]) => name), ["weekly"]);
  deepEqual(poolQuotaWindows({ ...pool, config: null }), []);
  // The old persisted plan is ignored, not a window filter or creation prerequisite.
  deepEqual(poolQuotaWindows({ ...pool, config: { ...newDevinPoolConfig(), plan: "max" } }).map(([name]) => name), ["daily", "weekly"]);
  equal(newDevinPoolConfig().strategy, "round_robin");
  equal(newDevinPoolConfig().reserve_policy, "disabled");
  equal("plan" in newDevinPoolConfig(), false);
});

test("mixed weighted snapshots and unknown capacity stay backend-owned", () => {
  const weighted: PoolQuotaWindow = {
    ...window, total_units: 12, total_accounts: 3, known_accounts: 3, known_remaining_units: 5.8,
    remaining_units: 5.8, used_units: 6.2, used_percent: 51.6667, remaining_percent: 48.3333,
  };
  const mixed = {
    ...pool, counts: { ...pool.counts, pro: 2, max: 1 },
    quota: { ...pool.quota, daily: { ...window, total_units: 2, total_accounts: 2 }, weekly: weighted },
  };
  const windows = poolQuotaWindows(mixed);
  equal(windows[0][1].total_units, 2);
  equal(windows[1][1], weighted);
  equal(windows[1][1].known_accounts, 3);
  equal(windows[1][1].next_reset_accounts, 1);
  const pending = { ...weighted, total_units: 0, capacity_complete: false };
  equal(poolQuotaWindows({ ...mixed, quota: { ...mixed.quota, weekly: pending } })[1][1], pending);
});

test("unconfigured legacy reauthorization allows unknown plans but refuses ambiguous or foreign targets", () => {
  const account = {
    account_id: "legacy", provider_id: "devin", label: "Teams", is_enabled: true, priority: 0,
    status: "connected", status_version: 1, verified_plan: null, identity_verified: false,
    scheduling: "unknown", daily: null, weekly: null, reserve_usd: null, observed_at: null, stale: true, updated_at: "",
  };
  const legacy: AccountPoolView = { ...pool, config: null, accounts: [account] };
  equal(canReauthorizeLegacyProvider(legacy, "devin"), true);
  equal(canReauthorizeLegacyProvider({ ...legacy, accounts: [] }, "devin"), true);
  equal(canReauthorizeLegacyProvider({ ...legacy, accounts: [{ ...account, status: "disconnected" }] }, "devin"), true);
  equal(canReauthorizeLegacyProvider({ ...legacy, config: newDevinPoolConfig() }, "devin"), false);
  // Tombstones count too: never filter disabled/disconnected accounts and pick one.
  equal(canReauthorizeLegacyProvider({ ...legacy, accounts: [account, { ...account, account_id: "other", status: "disconnected" }] }, "devin"), false);
  equal(canReauthorizeLegacyProvider(legacy, "another-provider"), false);
  equal(canReauthorizeLegacyProvider({ ...legacy, accounts: [{ ...account, provider_id: "another-provider" }] }, "devin"), false);
});

test("readiness is backend-owned, with English default and Chinese localization", () => {
  equal(accountStateLabel(pool.readiness, false), "Quota exhausted");
  equal(accountStateLabel("reserve_only", true), "仅付费余额可用");
  equal(accountStateLabel("future_status", false), "future_status");
  equal(localizeBackendErrorMessage("ACCOUNT_VERSION_CONFLICT: stale", false), "This account changed. Reload it and retry the operation.");
  equal(localizeBackendErrorMessage("POOL_NOT_CONFIGURED: enable", false), "Enable the account pool in settings before adding accounts. Account types are detected automatically.");
  equal(localizeBackendErrorMessage("POOL_NOT_CONFIGURED: enable", true), "添加账号前请先在设置中启用账号池，账号类型将自动识别。");
  equal(localizeBackendErrorMessage('{"code":"PLAN_MISMATCH","message":"detail"}', true), "此账号套餐与账号池档位不匹配。");
});
