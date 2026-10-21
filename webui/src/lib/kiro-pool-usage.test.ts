import { equal, ok } from "node:assert/strict";
import { test } from "node:test";
import { newDevinPoolConfig, type AccountPoolView, type KiroUsageItem, type ProviderAccountView } from "./devin-account-pool";
import { kiroPoolUsage, looksLikeCredential } from "./kiro-pool-usage";

const NOW = Date.parse("2026-10-21T10:00:00Z");
const RESET = "2026-11-01T00:00:00Z";
const close = (actual: number | null, expected: number, digits = 6) => {
  ok(actual != null, "expected a number");
  equal(Number(actual.toFixed(digits)), Number(expected.toFixed(digits)));
};

function account(id: string, items: KiroUsageItem[], overrides: Partial<ProviderAccountView> = {}): ProviderAccountView {
  return {
    account_id: id, provider_id: "kiro", label: id, is_enabled: true, priority: 0,
    status: "connected", status_version: 1, verified_plan: null, identity_verified: true,
    scheduling: "included", daily: null, weekly: null, reserve_usd: null,
    observed_at: NOW, stale: false, updated_at: "",
    kiro_usage: { plan: "PRO", email: null, resets_at: RESET, overage_enabled: false, items },
    ...overrides,
  };
}

const credit = (used: number, limit: number, resets_at: string | null = RESET, kind = "base"): KiroUsageItem =>
  ({ resource: "CREDIT", kind, name: "Credits", used, limit, resets_at });

function pool(accounts: ProviderAccountView[]): AccountPoolView {
  return {
    kind: "kiro", provider_id: "kiro", config: newDevinPoolConfig(), accounts,
    quota: { daily: null, weekly: null, estimated: false }, readiness: "eligible",
    counts: { total: accounts.length, accounting: accounts.length, available: accounts.length, included: accounts.length,
      reserve: 0, pro: 0, max: 0, unclassified: 0, unknown: 0, disabled: 0, disconnected: 0, auth_required: 0,
      quota_exhausted: 0, plan_mismatch: 0, plan_unverified: 0 },
  };
}

test("pool total sums each account's actual used and limit into one percent", () => {
  const summary = kiroPoolUsage(pool([account("pro-plus", [credit(1153.6, 2000)]), account("pro-max", [credit(0.05, 5000)])]), NOW);
  equal(summary.members, 2);
  equal(summary.observed, 2);
  equal(summary.resources.length, 1);
  const [resource] = summary.resources;
  equal(resource.resource, "CREDIT");
  close(resource.used, 1153.65);
  equal(resource.limit, 7000);
  close(resource.usedPercent, 1153.65 / 7000 * 100);
  close(resource.remainingPercent, 100 - 1153.65 / 7000 * 100);
  equal(resource.accounts, 2);
  equal(resource.nextResetAt, new Date(RESET).toISOString());
  equal(resource.nextResetAccounts, 2);
  equal(resource.resetAligned, true);
  // Aligned resets: the weighted pace is the shared monthly window progress.
  const remaining = Date.parse(RESET) - NOW;
  close(resource.pacePercent, (30 * 86400_000 - remaining) / (30 * 86400_000) * 100);
});

test("pace is weighted by each account's allowance and its own reset", () => {
  const day = 86400_000;
  const early = new Date(NOW + 3 * day).toISOString(); // 90% through its window
  const late = new Date(NOW + 27 * day).toISOString(); // 10% through its window
  const [resource] = kiroPoolUsage(pool([account("a", [credit(0, 1000, early)]), account("b", [credit(0, 3000, late)])]), NOW).resources;
  close(resource.pacePercent, (1000 * 90 + 3000 * 10) / 4000);
  equal(resource.resetAligned, false);
  equal(resource.nextResetAt, early);
  equal(resource.nextResetAccounts, 1);
});

test("active bonuses add capacity, expired grants do not, and overage is never capacity", () => {
  const items = [
    credit(120, 100),
    credit(10, 50, "2026-12-01T00:00:00Z", "bonus"),
    credit(40, 40, "2026-10-01T00:00:00Z", "free_trial"),
  ];
  const [resource] = kiroPoolUsage(pool([account("a", items)]), NOW).resources;
  equal(resource.used, 100 + 10);
  equal(resource.limit, 150);
  // Bonus expiry is not a base reset.
  equal(resource.nextResetAt, new Date(RESET).toISOString());
});

test("disabled and disconnected accounts are not members; unknown usage lowers coverage", () => {
  const summary = kiroPoolUsage(pool([
    account("live", [credit(50, 100)]),
    account("unknown", [], { kiro_usage: null }),
    account("off", [credit(100, 100)], { is_enabled: false }),
    account("gone", [credit(100, 100)], { status: "disconnected" }),
  ]), NOW);
  equal(summary.members, 2);
  equal(summary.observed, 1);
  equal(summary.resources[0].accounts, 1);
  equal(summary.resources[0].usedPercent, 50);
});

test("a contributing account without a reset hides the pool pace instead of guessing", () => {
  const [resource] = kiroPoolUsage(pool([
    account("a", [credit(10, 100)]),
    account("b", [credit(10, 100, null)], { kiro_usage: { plan: null, email: null, resets_at: null, overage_enabled: false, items: [credit(10, 100, null)] } }),
  ]), NOW).resources;
  equal(resource.pacePercent, null);
  equal(resource.resetAligned, false);
  equal(resource.nextResetAccounts, 1);
});

test("resources stay separate, zero allowances read exhausted, invalid numbers are ignored", () => {
  const summary = kiroPoolUsage(pool([account("a", [
    credit(5, 10),
    { resource: "AGENT", kind: "base", name: "Agent", used: 0, limit: 0, resets_at: RESET },
    { resource: "CREDIT", kind: "bonus", name: "bad", used: Number.NaN, limit: 10, resets_at: null },
  ])]), NOW);
  equal(summary.resources.map((r) => r.resource).join(","), "AGENT,CREDIT");
  equal(summary.resources[0].usedPercent, 100);
  equal(summary.resources[0].pacePercent, null);
  equal(summary.resources[1].limit, 10);
  equal(summary.resources[1].usedPercent, 50);
});

test("stale contributors are counted, not dropped", () => {
  const [resource] = kiroPoolUsage(pool([account("a", [credit(1, 10)], { stale: true }), account("b", [credit(1, 10)])]), NOW).resources;
  equal(resource.accounts, 2);
  equal(resource.staleAccounts, 1);
});

test("credential-shaped labels are detected, plain names are not", () => {
  for (const bad of [
    // Synthetic fixtures only: never paste real tokens into tests.
    '"refreshToken": "aorAAAAAFakeFixtureTokenNotReal0000000000',
    "aoaAAAAAFakeFixtureTokenNotReal1111",
    "ksk_abcdef123456",
    "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
    "client_secret=abc",
  ]) ok(looksLikeCredential(bad), bad);
  for (const good of ["Work", "Kiro 2", "GitHub pudvfxjv6k", "PRO MAX 账号", "refresh me later"]) ok(!looksLikeCredential(good), good);
});
