import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { isKiroOverage, providerUsageSupported, providerUsageTitle, summarizeGroupUsage, tierLabel } from "./provider-usage";
import type { ProviderUsage } from "./types";

const kiro = { vendor: "kiro", preset_key: "kiro", channel: "ide", auth_mode: "oauth" as const, base_url: "https://q.us-east-1.amazonaws.com" };

test("Kiro usage requires OAuth identity, not an Amazon Q URL alone", () => {
  equal(providerUsageSupported(kiro), true);
  equal(providerUsageSupported({ ...kiro, vendor: " KIRO ", preset_key: null }), true);
  equal(providerUsageSupported({ ...kiro, vendor: null }), true);
  equal(providerUsageSupported({ ...kiro, auth_mode: "apikey" }), false);
  equal(providerUsageSupported({ ...kiro, vendor: "custom", preset_key: null }), false);
  equal(providerUsageSupported({ ...kiro, vendor: "devin", preset_key: "devin" }), true);
});

test("Kiro plan title and quota breakdown labels are readable in both locales", () => {
  equal(providerUsageTitle("kiro_plan", false), "Kiro Usage");
  equal(providerUsageTitle("kiro_plan", true), "Kiro 用量");
  equal(tierLabel("monthly", false), "Monthly");
  equal(tierLabel("feature:kiro:base:Credits:0", false), "Base allowance · Credits");
  equal(tierLabel("feature:kiro:bonus:welcome_bonus:1", true), "赠送额度 · Welcome Bonus");
  equal(tierLabel("feature:kiro:free_trial:Free trial:2", false), "Free trial · Free Trial");
  equal(tierLabel("kiro:agentic_requests", false), "Agentic Requests · Total pool");
  equal(tierLabel("feature:code_review:weekly_limit", false), "Code Review · Weekly");
});

test("Only an eligible exhausted total Kiro pool displays upstream overage", () => {
  const usage: Pick<ProviderUsage, "kind" | "tiers" | "scheduling" | "is_available"> = {
    kind: "kiro_plan",
    tiers: [{ name: "monthly", used_percent: 100 }],
    scheduling: { status: "eligible", blocking_tiers: [] },
    is_available: true,
  };
  equal(isKiroOverage(usage), true);
  equal(isKiroOverage({ ...usage, scheduling: { status: "quota_exhausted", blocking_tiers: ["monthly"] } }), false);
  equal(isKiroOverage({ ...usage, scheduling: undefined }), false);
  equal(isKiroOverage({ ...usage, is_available: false }), false);
  equal(isKiroOverage({ ...usage, kind: "devin_plan" }), false);
  equal(isKiroOverage({ ...usage, tiers: [{ name: "monthly", used_percent: 50 }, { name: "feature:kiro:base:Credits:0", used_percent: 100 }] }), false);
  equal(isKiroOverage({ ...usage, tiers: [] }), false);
  equal(isKiroOverage(null), false);
});

const usage = (over: Partial<ProviderUsage>): ProviderUsage => ({
  provider_id: "p", kind: "x", site: "cn", tiers: [], queried_at: "", ...over,
});

test("group summary takes the worst main window per member and ignores feature tiers", () => {
  const s = summarizeGroupUsage([
    { name: "a", usage: usage({ tiers: [{ name: "five_hour", used_percent: 20 }, { name: "weekly_limit", used_percent: 55 }, { name: "feature:x:daily", used_percent: 99 }] }) },
    { name: "b", usage: usage({ tiers: [{ name: "monthly", used_percent: 130 }] }) },
  ]);
  equal(s.tracked, 2);
  equal(s.peak?.name, "b");
  equal(s.peak?.percent, 100);
  equal(s.perProvider.find((p) => p.name === "a")?.percent, 55);
});

test("group summary counts paused, unavailable, failed and loading members separately", () => {
  const s = summarizeGroupUsage([
    { name: "a", usage: usage({ scheduling: { status: "quota_exhausted", blocking_tiers: [] } }) },
    { name: "b", usage: usage({ is_available: false }) },
    { name: "c", failed: true },
    { name: "d", loading: true },
    { name: "e", usage: usage({ tiers: [{ name: "daily", used_percent: 10 }] }), failed: true },
  ]);
  deepEqual([s.paused, s.unavailable, s.failed, s.loading], [1, 1, 1, 1]);
  equal(s.peak?.percent, 10);
});

test("group summary sums balances per currency only for members without percent tiers", () => {
  const s = summarizeGroupUsage([
    { name: "a", usage: usage({ balances: [{ currency: "CNY", total: 10 } as never] }) },
    { name: "b", usage: usage({ balances: [{ currency: "CNY", total: 5.5 } as never, { currency: "USD", total: 1 } as never] }) },
    { name: "c", usage: usage({ tiers: [{ name: "daily", used_percent: 1 }], balances: [{ currency: "CNY", total: 999 } as never] }) },
  ]);
  deepEqual(s.balances, [{ currency: "CNY", total: 15.5 }, { currency: "USD", total: 1 }]);
  equal(summarizeGroupUsage([]).peak, null);
});
