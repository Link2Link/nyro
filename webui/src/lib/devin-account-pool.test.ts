import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { accountStateLabel, canReauthorizeLegacyProvider, newDevinPoolConfig, poolQuotaWindows, type AccountPoolView, type PoolQuotaWindow } from "./devin-account-pool";
import { localizeBackendErrorMessage } from "./backend-error";

const window: PoolQuotaWindow = {
  total_units: 3, known_accounts: 2, known_remaining_units: 1.5,
  used_units: null, remaining_units: null, used_percent: null, remaining_percent: null,
  stale: true, next_reset_at: "2030-01-01T01:00:00Z", next_reset_accounts: 1, reset_mode: "unknown",
};
const pool: AccountPoolView = {
  provider_id: "devin", config: newDevinPoolConfig("pro"), accounts: [],
  quota: { daily: window, weekly: window, estimated: true },
  counts: { total: 3, accounting: 3, available: 0, included: 0, reserve: 0, unknown: 0, disabled: 0, disconnected: 0, auth_required: 0, quota_exhausted: 3, plan_mismatch: 0, plan_unverified: 0 },
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

test("Max never renders a daily window, and legacy never invents a pool plan", () => {
  deepEqual(poolQuotaWindows({ ...pool, config: newDevinPoolConfig("max") }).map(([name]) => name), ["weekly"]);
  deepEqual(poolQuotaWindows({ ...pool, config: null }), []);
  equal(newDevinPoolConfig("max").reserve_policy, "disabled");
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
  equal(canReauthorizeLegacyProvider({ ...legacy, config: newDevinPoolConfig("pro") }, "devin"), false);
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
  equal(localizeBackendErrorMessage('{"code":"PLAN_MISMATCH","message":"detail"}', true), "此账号套餐或额度窗口与账号池不匹配。");
});
