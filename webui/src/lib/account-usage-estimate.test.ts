import { deepEqual, equal, ok } from "node:assert/strict";
import { test } from "node:test";
import { estimateAccountUsagePrice, type AccountUsageKind } from "./account-usage-estimate";
import type { KiroUsageItem, ProviderAccountView } from "./devin-account-pool";
import type { AccountUsageStats } from "./types";

const end = Date.parse("2026-11-07T11:00:00Z");
const start = end - 7 * 86_400_000;
const now = start + 86_400_000;
const reset = new Date(end).toISOString();
const totals = { total_tokens: 54_740_000, request_count: 3, equivalent_costs: [{ currency: "USD", total_cost: 21.4, priced_request_count: 3 }], unpriced_request_count: 0 };
const stats: AccountUsageStats = { account_id: "a", all_time: { ...totals, equivalent_costs: [{ currency: "USD", total_cost: 228.79, priced_request_count: 3 }] }, period: totals, period_start: start, period_end: end };
const item = (used: number, limit = 100, kind = "base", resets_at: string | null = reset, resource = "CREDIT"): KiroUsageItem => ({ resource, kind, name: resource, used, limit, resets_at });
const account: ProviderAccountView = {
  account_id: "a", provider_id: "p", label: "Account", is_enabled: true, priority: 0, status: "connected", status_version: 1,
  verified_plan: null, identity_verified: true, scheduling: "included", daily: { remaining_percent: 1, resets_at: reset },
  weekly: { remaining_percent: 90, resets_at: reset }, reserve_usd: null, stale: false, observed_at: now, updated_at: "",
  claude_usage: { windows: [
    { name: "five_hour", used_percent: 30, resets_at: reset },
    { name: "weekly_limit", used_percent: 10, resets_at: reset },
    { name: "feature:sonnet:weekly_limit", used_percent: 80, resets_at: reset },
  ] },
  kiro_usage: { plan: "PRO", email: null, resets_at: reset, overage_enabled: false, items: [item(10)] },
};
const project = (a = account, kind: AccountUsageKind = "devin", s = stats, at = now) => estimateAccountUsagePrice(s, a, kind, at);

test("all three account kinds project the current-cycle price at 100%, not all-time or remaining value", () => {
  for (const kind of ["devin", "claude_code", "kiro"] as const) {
    const result = project(account, kind);
    ok(result);
    equal(result.usedPercent, 10);
    equal(Number(result.costs[0].total_cost.toFixed(2)), 214);
  }
  deepEqual(stats.period, totals); // Never mutate shared query totals.
  equal(project({ ...account, weekly: { remaining_percent: 0, resets_at: reset } })?.costs[0].total_cost, 21.4);
  equal(project({ ...account, is_enabled: false })?.usedPercent, 10); // History remains useful for disabled accounts.
});

test("missing stale mismatched or invalid cycles never produce an estimate", () => {
  for (const a of [{ ...account, stale: true }, { ...account, account_id: "b" }, { ...account, weekly: null },
    { ...account, weekly: { remaining_percent: 90, resets_at: null } },
    { ...account, weekly: { remaining_percent: 90, resets_at: new Date(end + 1).toISOString() } }]) equal(project(a), null);
  for (const at of [start - 1, end, end + 1]) equal(project(account, "devin", stats, at), null);
  ok(project(account, "devin", stats, start));
  for (const s of [
    { ...stats, period: null }, { ...stats, period_start: null }, { ...stats, period_end: null },
    { ...stats, period_start: NaN }, { ...stats, period_end: Infinity }, { ...stats, period_start: end },
    { ...stats, period: { ...totals, request_count: 0 } },
    { ...stats, period: { ...totals, equivalent_costs: [] } },
  ]) equal(project(account, "devin", s), null);
});

test("zero, non-finite and out-of-range usage ratios are unavailable instead of dividing by zero", () => {
  for (const used of [0, -1, 101, NaN, Infinity]) {
    equal(project({ ...account, weekly: { remaining_percent: 100 - used, resets_at: reset } }), null);
    equal(project({ ...account, claude_usage: { windows: [{ name: "weekly_limit", used_percent: used, resets_at: reset }] } }, "claude_code"), null);
  }
  equal(project({ ...account, weekly: { remaining_percent: 50, resets_at: reset } }, "devin", {
    ...stats, period: { ...totals, equivalent_costs: [{ currency: "USD", total_cost: Number.MAX_VALUE, priced_request_count: 1 }] },
  }), null);
});

test("Claude requires exactly one matching main weekly window", () => {
  const windows = account.claude_usage!.windows;
  for (const selected of [[], [windows[0], windows[2]], [windows[1], windows[1]], [{ ...windows[1], resets_at: null }], [{ ...windows[1], resets_at: new Date(end + 1).toISOString() }]]) {
    equal(project({ ...account, claude_usage: { windows: selected } }, "claude_code"), null);
  }
});

test("projections retain native currencies, fractional precision, partial pricing and explicit priced zero", () => {
  const s = { ...stats, period: { ...totals, unpriced_request_count: 1, equivalent_costs: [
    { currency: "USD", total_cost: 0.001, priced_request_count: 1 },
    { currency: "CNY", total_cost: 7, priced_request_count: 1 },
  ] } };
  deepEqual(project(account, "devin", s)?.costs.map((cost) => [cost.currency, cost.total_cost]), [["USD", 0.01], ["CNY", 70]]);
  equal(s.period.equivalent_costs[0].total_cost, 0.001);
  equal(project(account, "devin", { ...stats, period: { ...totals, equivalent_costs: [{ currency: "USD", total_cost: 0, priced_request_count: 1 }] } })?.costs[0].total_cost, 0);
  for (const total_cost of [-1, NaN, Infinity]) equal(project(account, "devin", { ...stats, period: { ...totals, equivalent_costs: [{ currency: "USD", total_cost, priced_request_count: 1 }] } }), null);
});

test("Kiro uses a single resource's pooled base and active grants, not token or time percentages", () => {
  const kiro = (items: KiroUsageItem[], resets_at: string | null = reset) => ({ ...account, kiro_usage: { ...account.kiro_usage!, resets_at, items } });
  const monthly = { ...stats, period_start: Date.parse("2026-10-07T11:00:00Z") };
  const result = project(kiro([item(100), item(10, 100, "bonus"), item(50, 50, "free_trial", new Date(now).toISOString())]), "kiro", monthly);
  ok(result && Math.abs(result.usedPercent - 55) < 1e-10); // Expired trial excluded; active bonus included.
  equal(project(kiro([item(110)]), "kiro", monthly)?.usedPercent, 100); // Paid overage is not capacity.
  equal(project(kiro([item(10, 100, "base", null)]), "kiro", monthly)?.usedPercent, 10); // Global reset fallback.
  equal(project(kiro([item(10, 100, "free_trial")]), "kiro", monthly)?.usedPercent, 10);
  equal(project(kiro([item(10), item(1, 0, "free_trial", new Date(now - 1).toISOString(), "CHAT")]), "kiro", monthly)?.usedPercent, 10);
  for (const a of [
    { ...account, kiro_usage: null }, kiro([]), kiro([item(0)]), kiro([item(0, 0)]),
    kiro([item(10), item(5, 10, "base", reset, "CHAT")]),
    kiro([item(10, 100, "base", new Date(end + 1).toISOString())]),
    kiro([item(10), item(5, 100, "base", new Date(end + 1).toISOString())]),
    kiro([item(10, 100, "base", null)], null),
    kiro([item(NaN)]), kiro([item(1, Infinity)]), kiro([item(10), item(-1, 10, "bonus")]),
  ]) equal(project(a, "kiro", monthly), null);
});
