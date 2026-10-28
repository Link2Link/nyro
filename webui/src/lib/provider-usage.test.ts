import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { isKiroOverage, nyroUpstreamView, providerUsagePause, providerUsageSupported, providerUsageTitle, summarizeGroupUsage, tierLabel } from "./provider-usage";
import type { NyroUpstreamProvider, ProviderUsage } from "./types";

const kiro = { vendor: "kiro", preset_key: "kiro", channel: "ide", auth_mode: "oauth" as const, base_url: "https://q.us-east-1.amazonaws.com" };

test("Kiro usage requires OAuth identity, not an Amazon Q URL alone", () => {
  equal(providerUsageSupported(kiro), true);
  equal(providerUsageSupported({ ...kiro, vendor: " KIRO ", preset_key: null }), true);
  equal(providerUsageSupported({ ...kiro, vendor: null }), true);
  equal(providerUsageSupported({ ...kiro, auth_mode: "apikey" }), false);
  equal(providerUsageSupported({ ...kiro, vendor: "custom", preset_key: null }), false);
  equal(providerUsageSupported({ ...kiro, vendor: "devin", preset_key: "devin" }), true);
});

test("Claude subscription usage needs the Claude Code OAuth identity, not the API host", () => {
  const claude = { vendor: "anthropic", preset_key: "anthropic", channel: "claude-code", auth_mode: "oauth" as const, base_url: "https://api.anthropic.com" };
  equal(providerUsageSupported(claude), true);
  equal(providerUsageSupported({ ...claude, channel: "Claude-Code" }), true);
  equal(providerUsageSupported({ ...claude, auth_mode: "apikey" }), false);
  equal(providerUsageSupported({ ...claude, channel: "default", auth_mode: "apikey" }), false);
  equal(providerUsageTitle("claude_code_plan", false), "Claude Subscription Usage");
  equal(providerUsageTitle("claude_code_plan", true), "Claude 订阅用量");
  equal(tierLabel("feature:sonnet:weekly_limit", false), "Sonnet · Weekly");
  equal(tierLabel("feature:fable:weekly_limit", true), "Fable · 每周");
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

const pool: NonNullable<ProviderUsage["account_pool"]> = {
  provider_id: "p", config: null, accounts: [], readiness: "auth_required",
  quota: { daily: null, weekly: null, estimated: false },
  counts: { total: 1, accounting: 1, available: 0, included: 0, reserve: 0,
    pro: 0, max: 0, unclassified: 1, unknown: 0, disabled: 0, disconnected: 0,
    auth_required: 1, quota_exhausted: 0, plan_mismatch: 0, plan_unverified: 0 },
};
const resetAt = "2030-01-02T00:00:00Z";
const paused = { status: "quota_exhausted" as const, blocking_tiers: ["monthly"], reset_at: resetAt,
  next_check_at: "2030-01-01T00:05:00Z" };

test("routing pause labels use non-quota reasons in English by default and explicit Chinese", () => {
  for (const [reason, en, zh] of [
    ["auth_required", "Needs login", "需要登录"],
    ["account_unavailable", "Account unavailable", "账号不可用"],
    ["no_available_account", "Account unavailable", "账号不可用"],
    ["no_account", "No account", "无账号"],
    ["disabled", "Disabled", "已停用"],
    ["plan_mismatch", "Plan mismatch", "套餐不匹配"],
    ["plan_unverified", "Plan unverified", "套餐未验证"],
    ["plan_verification_required", "Plan verification required", "需要验证套餐"],
    ["unknown_usage", "Usage unknown", "用量未知"],
    ["future_reason", "future_reason", "future_reason"],
  ]) {
    const input = { scheduling: { ...paused, reason } };
    deepEqual(providerUsagePause(input), {
      message: `${en}, routing paused`, quotaExhausted: false, resetAt: null,
    });
    equal(providerUsagePause(input, true)?.message, `${zh}，已暂停调度`);
  }
});

test("pool readiness refines generic/legacy quota reasons but explicit non-quota reasons win", () => {
  for (const reason of [undefined, "account_unavailable", "no_available_account", "usage_limit", "quota_exhausted"]) {
    const result = providerUsagePause({ scheduling: { ...paused, reason }, account_pool: pool });
    equal(result?.message, "Needs login, routing paused");
    equal(result?.resetAt, null);
  }
  equal(providerUsagePause({ scheduling: { ...paused, reason: "auth_required" },
    account_pool: { ...pool, readiness: "quota_exhausted" } })?.quotaExhausted, false);
  equal(providerUsagePause({ scheduling: { ...paused, reason: "account_unavailable" },
    account_pool: { ...pool, readiness: "plan_verification_required" } })?.message,
  "Plan verification required, routing paused");
});

test("only quota pauses expose real reset times, never next checks", () => {
  for (const reason of ["usage_limit", "quota_exhausted", undefined]) {
    deepEqual(providerUsagePause({ scheduling: { ...paused, reason } }), {
      message: "Quota exhausted, routing paused", quotaExhausted: true, resetAt,
    });
    equal(providerUsagePause({ scheduling: { ...paused, reason, reset_at: null } })?.resetAt, null);
    equal(providerUsagePause({ scheduling: { ...paused, reason } }, true)?.message, "额度已满，已暂停调度");
  }
  equal(providerUsagePause({ scheduling: { ...paused, blocking_tiers: [] } })?.quotaExhausted, false);
  equal(providerUsagePause({ scheduling: { ...paused, reason: "account_unavailable" },
    account_pool: { ...pool, readiness: "quota_exhausted" } })?.quotaExhausted, true);
  equal(providerUsagePause({ scheduling: { ...paused, status: "eligible", reason: "auth_required" }, account_pool: pool }), null);
  equal(providerUsagePause({ account_pool: pool }), null);
  equal(providerUsagePause(null), null);
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

const upstream = (name: string, over: Partial<NyroUpstreamProvider> = {}): NyroUpstreamProvider => ({
  id: name, name, routes: ["r"], status: "ok", ...over,
});

test("a chained Nyro is a usage provider by identity, whatever its address", () => {
  const nyro = { vendor: "nyro-gateway", preset_key: "nyro-gateway", channel: "default", auth_mode: "apikey" as const, base_url: "http://10.0.0.2:19530" };
  equal(providerUsageSupported(nyro), true);
  equal(providerUsageSupported({ ...nyro, vendor: null, preset_key: " Nyro-Gateway " }), true);
  // The retired `nyro` id is the Custom preset, never the chain preset.
  equal(providerUsageSupported({ ...nyro, vendor: "nyro", preset_key: "nyro" }), false);
  equal(providerUsageTitle("nyro_gateway", false), "Upstream Nyro Usage");
  equal(providerUsageTitle("nyro_gateway", true), "上游 Nyro 用量");
  equal(tierLabel("nyro_upstream", false), "All upstream providers paused");
});

test("upstream rows put paused first, then the most used, then failures; unsupported become names", () => {
  const view = nyroUpstreamView({
    redacted: ["balances", "spends"],
    providers: [
      upstream("low", { usage: usage({ tiers: [{ name: "weekly_limit", used_percent: 10 }] }) }),
      upstream("relay", { status: "unsupported" }),
      upstream("broken", { status: "error", error: "HTTP 500" }),
      upstream("high", { usage: usage({ tiers: [{ name: "five_hour", used_percent: 90 }] }) }),
      upstream("paused", { usage: usage({ scheduling: { status: "quota_exhausted", blocking_tiers: ["daily"] } }) }),
      upstream("balance-only", { usage: usage({}) }),
    ],
  });
  deepEqual(view.shown.map((p) => p.name), ["paused", "high", "low", "balance-only", "broken"]);
  deepEqual(view.unsupported, ["relay"]);
  deepEqual([view.total, view.paused, view.failed, view.moneyRedacted], [6, 1, 1, true]);
  equal(nyroUpstreamView(null).total, 0);
  equal(nyroUpstreamView({ providers: [] }).moneyRedacted, false);
});

test("group summary reads a chained Nyro's upstream windows, never its remote balances", () => {
  const report = { providers: [
    upstream("kimi", { usage: usage({ tiers: [{ name: "five_hour", used_percent: 72 }] }) }),
    upstream("deepseek", { usage: usage({ balances: [{ currency: "CNY", total: 50 } as never] }) }),
  ] };
  const s = summarizeGroupUsage([{ name: "edge", usage: usage({ kind: "nyro_gateway", upstream: report }) }]);
  equal(s.peak?.name, "edge");
  equal(s.peak?.percent, 72);
  const balancesOnly = summarizeGroupUsage([{ name: "edge", usage: usage({ kind: "nyro_gateway", balances: [{ currency: "CNY", total: 1 } as never], upstream: { providers: [] } }) }]);
  deepEqual(balancesOnly.balances, []);
});
