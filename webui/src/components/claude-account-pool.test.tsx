import { doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { LocaleProvider } from "../lib/i18n";
import { newDevinPoolConfig, type AccountPoolView, type PoolQuotaWindow, type ProviderAccountView } from "../lib/devin-account-pool";
import type { AccountUsageStats, Provider } from "../lib/types";
import { accountUsagePeriodStart } from "../lib/account-usage-stats";
import { AccountPoolSettings } from "./devin-pool-settings";
import { ClaudeAccountPoolCard, ClaudeAccountPoolPanel, ClaudeAccountUsage, ClaudePoolSummary } from "./claude-account-pool";
import { ClaudeAccountLogin } from "./claude-account-login";

const account: ProviderAccountView = {
  account_id: "account-a", provider_id: "claude", label: "Work", is_enabled: true, priority: 2,
  status: "connected", status_version: 4, verified_plan: null, identity_verified: true,
  scheduling: "included", daily: null, weekly: null, reserve_usd: null,
  observed_at: Date.parse("2030-01-01T00:00:00Z"), stale: false, updated_at: "",
  claude_usage: { windows: [
    { name: "five_hour", used_percent: 37.5, resets_at: "2030-01-01T05:00:00Z" },
    { name: "weekly_limit", used_percent: 12, resets_at: "2030-01-07T00:00:00Z" },
    { name: "feature:sonnet:weekly_limit", used_percent: 4, resets_at: null },
    { name: "feature:fable:weekly_limit", used_percent: 0, resets_at: null },
  ] },
};
const pool: AccountPoolView = {
  kind: "claude_code", provider_id: "claude", config: newDevinPoolConfig(), accounts: [account],
  quota: { daily: null, weekly: null, estimated: false }, readiness: "eligible",
  counts: { total: 1, accounting: 1, available: 1, included: 1, reserve: 0, pro: 0, max: 0, unclassified: 1,
    unknown: 0, disabled: 0, disconnected: 0, auth_required: 0, quota_exhausted: 0, plan_mismatch: 0, plan_unverified: 0 },
};
const provider: Provider = {
  id: "claude", name: "Claude", vendor: "anthropic", channel: "claude-code", protocol: "anthropic", auth_mode: "oauth",
  base_url: "https://api.anthropic.com", use_proxy: false, fast_mode: false, is_enabled: true, created_at: "", updated_at: "",
};
function panel(value = pool, card = false, stats?: AccountUsageStats[], inspect?: (client: QueryClient) => void) {
  const client = new QueryClient();
  client.setQueryData(["provider-account-pool", "claude"], value);
  if (stats) client.setQueryData(["provider-account-usage-stats", "claude", accountUsagePeriodStart("week", Date.now())], stats);
  try {
    const html = renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      {card ? <ClaudeAccountPoolCard provider={provider} /> : <ClaudeAccountPoolPanel provider={provider} />}
    </LocaleProvider></QueryClientProvider>);
    inspect?.(client);
    return html;
  } finally { client.clear(); }
}

test("Claude management shows actual 5h weekly and model windows with scoped controls", () => {
  const html = panel();
  for (const text of ["Claude Code account pool", "Add account", "Re-authorize", "Refresh usage", "Refresh pool usage", "Disable", "Remove", "Account label", "Account priority", "account-a · v4", "5h", "Weekly", "Sonnet · Weekly", "Fable · Weekly", "Used 37.5%", "Used 0.0%", "Reset time unknown"]) match(html, new RegExp(text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  equal((html.match(/role="progressbar"/g) ?? []).length, 4);
  match(html, /dateTime="2030-01-01T05:00:00Z"/);
  doesNotMatch(html, /Devin|Pro-equivalent|Team Full Seat|10×|Paid balance|Plan unverified|Kiro/);
});

test("account rows preserve detected plans and disclose the unknown-plan Pro fallback", () => {
  for (const [plan, label] of [["pro", "Pro"], ["max 5x", "Max 5×"], ["max 20x", "Max 20×"], [null, "Plan unknown"]]) {
    const html = panel({ ...pool, accounts: [{ ...account, claude_usage: { plan, windows: [] } }] });
    match(html, new RegExp(label!));
    if (plan == null) match(html, /Plan unknown \(counted as Pro\)/);
  }
});

test("pool summary labels detected plan counts with explicit Pro fallback", () => {
  const accounts = ["pro", "pro", "max 5x", "max 20x", null, "team"].map((plan, i) => ({
    ...account, account_id: `account-${i}`, claude_usage: { plan, windows: [] },
  }));
  accounts.push({ ...accounts[0], account_id: "disabled", is_enabled: false });
  accounts.push({ ...accounts[0], account_id: "disconnected", status: "disconnected" });
  for (const isZh of [false, true]) {
    const html = renderToStaticMarkup(<ClaudePoolSummary pool={{ ...pool, accounts }} isZh={isZh} detailed />);
    match(html, /Pro · 2/); match(html, /Max 5× · 1/); match(html, /Max 20× · 1/);
    match(html, isZh ? /汇总账号套餐：/ : /Plans in pool totals:/);
    match(html, isZh ? /套餐未知 \(按 Pro 计算\) · 1/ : /Plan unknown \(counted as Pro\) · 1/);
    match(html, isZh ? /Team \(按 Pro 计算\) · 1/ : /Team \(counted as Pro\) · 1/);
  }
  const empty = renderToStaticMarkup(<ClaudePoolSummary pool={{ ...pool, accounts: [] }} isZh={false} detailed />);
  match(empty, /No accounts in totals/);
});

test("Claude settings expose strategy affinity attempts but no paid reserve switch", () => {
  const html = renderToStaticMarkup(<LocaleProvider><AccountPoolSettings vendor="claude_code" config={{ ...newDevinPoolConfig(), reserve_policy: "fallback" }} pending={false} onSave={() => {}} /></LocaleProvider>);
  for (const text of ["Scheduling strategy", "Round robin", "Fill first", "Session affinity TTL", "Maximum account attempts", "Enable session affinity", "Paid extra-usage fallback is disabled", "not a billing hard limit"]) match(html, new RegExp(text));
  equal((html.match(/type="checkbox"/g) ?? []).length, 1);
  doesNotMatch(html, /Allow paid|Pro \/ Team|10×/);
});

test("legacy single-account login remains enabled while adding requires pool settings", () => {
  const html = panel({ ...pool, config: null });
  const button = html.match(/<button[^>]*>Re-authorize legacy account<\/button>/)?.[0] ?? "";
  match(button, /legacy account/);
  doesNotMatch(button, /disabled=""/);
  match(html, /<button[^>]*disabled=""[^>]*>Add account<\/button>/);
  match(html, /Save the default settings to enable the pool/);
});

test("disconnected Claude records retain stats but disallow account mutations", () => {
  const html = panel({ ...pool, accounts: [{ ...account, status: "disconnected", scheduling: "disconnected", is_enabled: false, claude_usage: null }] });
  for (const label of ["Refresh usage", "Re-authorize", "Enable", "Remove"]) match(html.match(new RegExp(`<button[^>]*>${label}</button>`))?.[0] ?? "", /disabled=""/);
  match(html, /Usage unknown/);
  doesNotMatch(html, /role="progressbar"/);
});

const weightedWindow: PoolQuotaWindow = {
  total_units: 26, total_accounts: 3, capacity_complete: true, known_accounts: 3,
  known_remaining_units: 22.5, used_units: 3.5, remaining_units: 22.5,
  used_percent: 3.5 / 26 * 100, remaining_percent: 22.5 / 26 * 100,
  stale: false, next_reset_at: "2030-01-01T05:00:00Z", next_reset_accounts: 1, reset_mode: "independent",
};
const weightedPool: AccountPoolView = { ...pool, quota: { ...pool.quota, estimated: true, claude_windows: [
  { name: "five_hour", quota: weightedWindow },
  { name: "weekly_limit", quota: { ...weightedWindow, total_units: 16, used_units: 2.5, remaining_units: 13.5,
    known_remaining_units: 13.5, used_percent: 15.625, remaining_percent: 84.375 } },
] } };

test("Claude collapsed card shows compact weighted usage without management details", () => {
  for (const isZh of [false, true]) {
    const html = renderToStaticMarkup(<ClaudePoolSummary pool={weightedPool} isZh={isZh} onRefresh={() => {}} refreshing />);
    match(html, isZh ? />5小时<\/span>/ : />5h<\/span>/);
    match(html, isZh ? />每周<\/span>/ : />Weekly<\/span>/);
    match(html, isZh ? /已用 13.5%/ : /Used 13.5%/);
    match(html, isZh ? /已用 15.6%/ : /Used 15.6%/);
    equal((html.match(/role="progressbar"/g) ?? []).length, 2);
    match(html, /aria-busy="true"/);
    match(html, /disabled=""/);
    doesNotMatch(html, /data-claude-pool-plans|Weighted estimate|加权估算|Pro-equivalent|Pro 等价|Known usage|有效用量|Next reset|最近重置|Disabled|已停用|accounts ·|个账号/);
  }
  const html = panel(weightedPool, true);
  match(html, /aria-label="Refresh pool usage"/);
  match(html, /aria-expanded="false"[^>]*>Manage accounts/);
  equal((html.match(/role="progressbar"/g) ?? []).length, 2);
  doesNotMatch(html, /data-steady-pace-marker|Sonnet · Weekly|Weighted estimate/);
});

test("Claude management preserves detailed core-weighted session and weekly estimates", () => {
  const html = panel(weightedPool);
  match(html, /5h · Weighted estimate/);
  match(html, /Weekly · Weighted estimate/);
  match(html, /Used 13.5%/); match(html, /Used 15.6%/);
  match(html, /Used 3.50 \/ 26.00 Pro-equivalent units · Remaining 22.50/);
  match(html, /Used 2.50 \/ 16.00 Pro-equivalent units · Remaining 13.50/);
  match(html, /5h 1 \/ 5 \/ 20; weekly 1 \/ 5 \/ 10/);
  match(html, /Unknown or unrecognized plans default to Pro \(1\)/);
  match(html, /Known usage 3 \/ 3 accounts/);
  match(html, /not necessarily the whole pool/);
  match(html, /Plans in pool totals:/);
  match(html, /Sonnet · Weekly/);
  equal((html.match(/role="progressbar"/g) ?? []).length, 6);
});

test("Claude pool windows stack vertically and show weighted steady-pace markers", () => {
  const now = Date.now();
  const accounts = ["pro", "max 5x", "max 20x"].map((plan, index) => ({
    ...account, account_id: `pace-${index}`, claude_usage: { plan, windows: [
      { name: "five_hour", used_percent: 20, resets_at: new Date(now + 2.5 * 3600_000).toISOString() },
      { name: "weekly_limit", used_percent: 30, resets_at: new Date(now + 3.5 * 24 * 3600_000).toISOString() },
    ] },
  }));
  const value = { ...weightedPool, accounts };
  const html = panel(value, true);
  match(html, /grid grid-cols-1 gap-2/);
  doesNotMatch(html, /sm:grid-cols-2/);
  equal((html.match(/data-steady-pace-marker="weighted"/g) ?? []).length, 2);
  match(html, /Steady pace: 50.0%/);
  doesNotMatch(html, /Steady-pace estimate/);
  match(html, /Claude plan capacity/);
  const compactZh = renderToStaticMarkup(<ClaudePoolSummary pool={value} isZh />);
  match(compactZh, /匀速参考线：50.0%/);
  doesNotMatch(compactZh, /匀速预估用量/);
  const zh = renderToStaticMarkup(<ClaudePoolSummary pool={value} isZh detailed />);
  match(zh, /匀速预估用量 50.0%/); match(zh, /进度条竖线，仅供参考/);
  const incomplete = panel({ ...value, accounts: accounts.map((a) => ({ ...a, claude_usage: {
    ...a.claude_usage, windows: a.claude_usage.windows.map((w) => ({ ...w, resets_at: null })),
  } })) }, true);
  doesNotMatch(incomplete, /data-steady-pace-marker|Steady-pace estimate/);
  equal((incomplete.match(/role="progressbar"/g) ?? []).length, 2);
});

test("Claude summary does not average account percentages on older backend responses", () => {
  const html = panel({ ...pool, accounts: [account, { ...account, account_id: "account-b", claude_usage: { windows: [{ name: "five_hour", used_percent: 90, resets_at: null }] } }] }, true);
  match(html, /Pool usage unknown; refresh usage/);
  doesNotMatch(html, /role="progressbar"|Used [\d.]+%/);
});

test("incomplete unknown stale and invalid pool totals never display a fabricated percentage", () => {
  for (const patch of [
    { capacity_complete: false }, { known_accounts: 2 }, { stale: true },
    { used_percent: null }, { used_percent: NaN }, { used_percent: Infinity }, { used_percent: -1 }, { used_percent: 101 },
  ]) {
    const html = renderToStaticMarkup(<ClaudePoolSummary pool={{ ...pool, quota: { ...pool.quota,
      claude_windows: [{ name: "five_hour", quota: { ...weightedWindow, ...patch } }],
    } }} isZh={false} />);
    match(html, /Usage unknown \/ incomplete data/);
    doesNotMatch(html, /role="progressbar"|Used [\d.]+%|NaN|Infinity/);
  }
});

test("missing pool data does not hide a complete independent window", () => {
  const html = panel({ ...weightedPool, quota: { ...weightedPool.quota, claude_windows: [
    { name: "five_hour", quota: weightedWindow },
    { name: "weekly_limit", quota: { ...weightedWindow, used_percent: null, known_accounts: 1, stale: true, next_reset_at: "invalid" } },
  ] } }, true);
  equal((html.match(/role="progressbar"/g) ?? []).length, 1);
  match(html, /Usage unknown \/ incomplete data/);
  doesNotMatch(html, /Invalid Date/);
});

test("unknown invalid and stale Claude windows never invent fresh or zero capacity", () => {
  const unknown = renderToStaticMarkup(<ClaudeAccountUsage account={{ ...account, claude_usage: undefined, stale: true }} isZh={false} />);
  match(unknown, /Usage unknown/); match(unknown, /Stale \/ unknown/);
  doesNotMatch(unknown, /progressbar|Used 0/);
  const invalid = renderToStaticMarkup(<ClaudeAccountUsage account={{ ...account, observed_at: NaN, claude_usage: { windows: [{ name: "five_hour", used_percent: NaN, resets_at: "invalid" }] } }} isZh={false} />);
  doesNotMatch(invalid, /progressbar|Invalid Date|NaN|Infinity/);
  const stale = renderToStaticMarkup(<ClaudeAccountUsage account={{ ...account, stale: true, claude_usage: { windows: [{ name: "weekly_limit", used_percent: 100, resets_at: "2000-01-01T00:00:00Z" }] } }} isZh={false} />);
  match(stale, /Used 100.0%/); match(stale, /Stale \/ unknown/);
});

test("Claude window names are escaped and Chinese labels are explicit", () => {
  const html = renderToStaticMarkup(<ClaudeAccountUsage account={account} isZh />);
  for (const text of ["5小时", "每周", "已用", "重置", "已观察"]) match(html, new RegExp(text));
  const summary = renderToStaticMarkup(<ClaudePoolSummary pool={weightedPool} isZh detailed />);
  for (const text of ["加权估算", "已用 13.5%", "已用 15.6%", "5 小时 1 / 5 / 20", "每周 1 / 5 / 10", "不一定是全池", "有效用量 3 / 3 个账号", "未知或未识别套餐默认按 Pro"]) match(summary, new RegExp(text));
  const unsafe = renderToStaticMarkup(<ClaudeAccountUsage account={{ ...account, claude_usage: { windows: [{ name: '<script>alert(1)</script>', used_percent: 1, resets_at: null }] } }} isZh={false} />);
  doesNotMatch(unsafe, /<script>/i); match(unsafe, /&lt;Script&gt;/);
});

test("Claude management preserves retained and calendar-week usage stats without Kiro units", () => {
  const zero = { total_tokens: 0, request_count: 0, equivalent_costs: [], unpriced_request_count: 0 };
  const stats: AccountUsageStats[] = [{ account_id: "account-a", all_time: { ...zero, total_tokens: 2_500_000, request_count: 1, equivalent_costs: [{ currency: "USD", total_cost: 7.5, priced_request_count: 1 }] }, period: zero }];
  const html = panel(pool, false, stats);
  match(html, /data-account-usage="account-a"[\s\S]*2\.50M[\s\S]*\$7\.50[\s\S]*This week[\s\S]*\$0\.00/);
  match(html, />Remove<\/button><\/div><div data-account-usage=/);
  match(html, /calendar week starts Monday/);
  doesNotMatch(html, /This month|Kiro tokens|Loading totals/);
  panel(pool, true, undefined, (client) => equal(client.getQueryCache().findAll({ queryKey: ["provider-account-usage-stats"] }).length, 0));
  panel({ ...pool, accounts: [account, { ...account, account_id: "account-b" }] }, false, undefined, (client) => equal(client.getQueryCache().findAll({ queryKey: ["provider-account-usage-stats"] }).length, 1));
});

test("Claude login explains browser code exchange and exact target, not Devin token import", () => {
  const html = renderToStaticMarkup(<LocaleProvider><ClaudeAccountLogin intent={{ kind: "reauthorize_account", providerId: "claude", accountId: "account-a", expectedVersion: 4 }} initialLabel="Work" onBound={() => {}} onCancel={() => {}} /></LocaleProvider>);
  match(html, /Only the selected account/); match(html, /authorization code or full callback URL/);
  match(html, /Start authorization/); doesNotMatch(html, /Devin|session token|value="import"/);
});
