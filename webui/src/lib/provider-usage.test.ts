import { equal } from "node:assert/strict";
import { test } from "node:test";
import { isKiroOverage, providerUsageSupported, providerUsageTitle, tierLabel } from "./provider-usage";
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
