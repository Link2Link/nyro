import { deepEqual, doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { LocaleProvider } from "../lib/i18n";
import { newDevinPoolConfig, type AccountPoolView, type PoolQuotaWindow, type ProviderAccountView } from "../lib/devin-account-pool";
import type { AccountUsageStats, Provider } from "../lib/types";
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
  if (stats) client.setQueryData(["provider-account-usage-stats", "claude"], stats);
  try {
    const html = renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      {card ? <ClaudeAccountPoolCard provider={provider} /> : <ClaudeAccountPoolPanel provider={provider} />}
    </LocaleProvider></QueryClientProvider>);
    inspect?.(client);
    return html;
  } finally { client.clear(); }
}

const pad = (value: number) => String(value).padStart(2, "0");
/** Independent oracle for the local short time format. */
function localShort(epochMs: number, now: number, seconds = false): string {
  const date = new Date(epochMs);
  const time = `${pad(date.getHours())}:${pad(date.getMinutes())}${seconds ? `:${pad(date.getSeconds())}` : ""}`;
  return date.toDateString() === new Date(now).toDateString() ? time : `${date.getMonth() + 1}/${date.getDate()} ${time}`;
}
/** Balanced <div> starting at `start` (SSR never self-closes divs). */
function element(html: string, start: number): string {
  const tag = /<(\/?)div\b[^>]*>/g;
  tag.lastIndex = start;
  let depth = 0;
  for (let match = tag.exec(html); match; match = tag.exec(html)) {
    depth += match[1] ? -1 : 1;
    if (depth === 0) return html.slice(start, tag.lastIndex);
  }
  return html.slice(start);
}
function windowRows(html: string): Map<string, string> {
  const rows = new Map<string, string>();
  for (const match of html.matchAll(/<div[^>]*data-claude-window="([^"]*)"/g)) rows.set(match[1], element(html, match.index!));
  return rows;
}
const withoutTitles = (html: string) => html.replace(/ title="[^"]*"/g, "");
const textOf = (html: string) => html.replace(/<[^>]*>/g, " ");


test("Claude management shows actual 5h weekly and model windows with scoped controls", () => {
  const html = panel();
  for (const text of ["Claude Code account pool", "Add account", "Re-authorize", "Refresh usage", "Refresh pool usage", "Disable", "Remove", "Account label", "Account priority", "account-a · v4", "5h", "Weekly", "Sonnet · Weekly", "Fable · Weekly"]) match(html, new RegExp(text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  const rows = windowRows(html);
  deepEqual([...rows.keys()], ["five_hour", "weekly_limit", "feature:sonnet:weekly_limit", "feature:fable:weekly_limit"]);
  match(rows.get("five_hour")!, />37.5%</); match(rows.get("weekly_limit")!, />12%</);
  match(rows.get("feature:fable:weekly_limit")!, />0%</);
  equal((html.match(/role="progressbar"/g) ?? []).length, 4);
  match(html, /dateTime="2030-01-01T05:00:00Z"/);
  doesNotMatch(html, /Devin|Pro-equivalent|Team Full Seat|10×|Paid balance|Plan unverified|Kiro|Full usage snapshot|Reset time unknown/);
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

test("Claude refresh honors the backend deadline boundary without blocking other actions", (t) => {
  const deadline = Date.parse("2030-01-01T00:10:00Z");
  const value = { ...pool, accounts: [{ ...account, stale: true, usage_refresh_available_at: deadline }] };
  let now = deadline - 1;
  t.mock.method(Date, "now", () => now);
  for (const time of [deadline - 1, deadline, deadline + 1]) {
    now = time;
    const html = panel(value);
    const refresh = html.match(/<button[^>]*>Refresh usage<\/button>/)?.[0] ?? "";
    match(refresh, /Refresh usage/);
    match(refresh, /title="At most once every 10 min per account \(failures included\)"/);
    equal(refresh.includes('disabled=""'), time < deadline);
    equal((html.match(/data-claude-refresh-cooldown/g) ?? []).length, time < deadline ? 1 : 0);
    if (time < deadline) match(html, new RegExp(`Next refresh <time dateTime="2030-01-01T00:10:00.000Z"[^>]*>${localShort(deadline, time)}</time>`));
    doesNotMatch(html, /Usage refresh cooldown|can be attempted now|No refresh cooldown reported/);
    for (const label of ["Re-authorize", "Disable", "Remove"]) {
      const button = html.match(new RegExp(`<button[^>]*>${label}</button>`))?.[0] ?? "";
      match(button, new RegExp(label));
      doesNotMatch(button, /disabled=""/);
    }
    for (const label of ["Account label", "Account priority"]) {
      const input = html.match(new RegExp(`<input[^>]*aria-label="${label}"[^>]*>`))?.[0] ?? "";
      match(input, new RegExp(label));
      doesNotMatch(input, /disabled=""/);
    }
    // A failed/old full query keeps values visible; each full-query window gets one short mark.
    equal((html.match(/data-claude-stale-mark/g) ?? []).length, 4);
    match(html, />37.5%</);
  }
});

test("Claude cooldown uses the attempt deadline, not observation freshness, and is account-scoped", (t) => {
  const now = Date.parse("2030-01-01T01:00:00Z");
  t.mock.method(Date, "now", () => now);
  const html = panel({ ...pool, accounts: [
    { ...account, stale: true, usage_refresh_available_at: now + 60_000 },
    { ...account, account_id: "account-b", observed_at: now, usage_refresh_available_at: now },
  ] });
  const refreshButtons = html.match(/<button[^>]*>Refresh usage<\/button>/g) ?? [];
  equal(refreshButtons.length, 2);
  match(refreshButtons[0], /disabled=""/);
  doesNotMatch(refreshButtons[1], /disabled=""/);
});

test("Claude missing or invalid cooldown metadata never invents a deadline from observed_at", (t) => {
  const now = Date.parse("2030-01-01T00:00:00Z");
  t.mock.method(Date, "now", () => now);
  for (const deadline of [undefined, null, NaN, Infinity, 8_640_000_000_000_001]) {
    const html = panel({ ...pool, accounts: [{ ...account, observed_at: now, usage_refresh_available_at: deadline }] });
    const refresh = html.match(/<button[^>]*>Refresh usage<\/button>/)?.[0] ?? "";
    match(refresh, /Refresh usage/);
    doesNotMatch(refresh, /disabled=""/);
    doesNotMatch(html, /data-claude-refresh-cooldown|Next refresh|Invalid Date|NaN|Infinity/);
  }
});

test("Claude cooldown is a single short line without policy paragraphs in both languages", (t) => {
  const now = Date.parse("2030-01-01T00:00:00Z");
  const deadline = Date.parse("2030-01-01T00:10:00Z");
  t.mock.method(Date, "now", () => now);
  for (const isZh of [false, true]) {
    const html = renderToStaticMarkup(<ClaudeAccountUsage account={{ ...account, claude_usage: null,
      stale: true, observed_at: null, usage_refresh_available_at: deadline,
    }} isZh={isZh} />);
    const lines = html.match(/<p[^>]*data-claude-refresh-cooldown[^>]*>[\s\S]*?<\/p>/g) ?? [];
    equal(lines.length, 1);
    match(lines[0], new RegExp(`>${isZh ? "下次可刷新" : "Next refresh"} <time dateTime="2030-01-01T00:10:00.000Z"[^>]*>${localShort(deadline, now)}</time></p>`));
    match(html, isZh ? /用量未知/ : /Usage unknown/);
    equal((html.match(/<p\b/g) ?? []).length, 2);
    doesNotMatch(html, /progressbar/);
    doesNotMatch(textOf(html), /10 分钟|10 minutes|10 min|退避|backoff|冷却中|cooldown|完整用量快照|Full usage snapshot|被动|Passive|已观察|Observed/);
  }
});

test("Claude pool refresh stays enabled during cooldown and keeps its policy in hover text only", (t) => {
  const now = Date.parse("2030-01-01T00:00:00Z");
  t.mock.method(Date, "now", () => now);
  const value = { ...pool, accounts: [{ ...account, usage_refresh_available_at: now + 600_000 }] };
  for (const card of [false, true]) {
    const html = panel(value, card);
    const refresh = html.match(/<button[^>]*>(?:(?!<\/button>)[\s\S])*Refresh pool usage<\/button>/)?.[0]
      ?? html.match(/<button[^>]*aria-label="Refresh pool usage"[^>]*>/)?.[0] ?? "";
    match(refresh, /Refresh pool usage/);
    doesNotMatch(refresh, /disabled=""/);
    match(refresh, /title="[^"]*Each account refreshes at most every 10 min; accounts in cooldown return cached data"/);
    doesNotMatch(withoutTitles(html), /at least 10 minutes|10 min|cached data|bypasses the limit|Passive inference-header|contribute to pool totals/);
  }
  const zh = renderToStaticMarkup(<ClaudePoolSummary pool={value} isZh onRefresh={() => {}} />);
  match(zh, /aria-label="刷新全池用量"/);
  match(zh, /title="刷新全池用量\n每个账号至少间隔 10 分钟刷新；冷却中的账号返回缓存数据"/);
  doesNotMatch(withoutTitles(zh), /10 分钟|缓存|推理响应头|不参与账号池汇总|退避/);
});


test("effective Claude windows show response-header values once, with source and time only in hover text", (t) => {
  const now = Date.parse("2030-01-01T00:04:00Z");
  t.mock.method(Date, "now", () => now);
  const observed = Date.parse("2030-01-01T00:02:00Z");
  const subject: ProviderAccountView = { ...account, stale: true, claude_usage: { windows: [
    { name: "five_hour", used_percent: 82.5, resets_at: "2030-01-01T05:00:00Z", source: "inference_header", observed_at: observed },
    { name: "weekly_limit", used_percent: 12, resets_at: "2030-01-07T00:00:00Z", source: "usage_api", observed_at: account.observed_at },
    { name: "feature:sonnet:weekly_limit", used_percent: 4, resets_at: null },
  ] } };
  const original = structuredClone(subject);
  for (const isZh of [false, true]) {
    const html = renderToStaticMarkup(<ClaudeAccountUsage account={subject} isZh={isZh} />);
    const rows = windowRows(html);
    deepEqual([...rows.keys()], ["five_hour", "weekly_limit", "feature:sonnet:weekly_limit"]);
    for (const name of rows.keys()) equal(html.split(`data-claude-window="${name}"`).length - 1, 1);
    const five = rows.get("five_hour")!;
    match(five, /data-claude-source="inference_header"/);
    match(five, new RegExp(`title="${isZh ? "来自对话响应" : "From chat responses"} · ${localShort(observed, now, true)}"`));
    match(five, />82.5%</); match(five, /aria-valuenow="82.5"/);
    match(five, new RegExp(`<time dateTime="2030-01-01T05:00:00Z"[^>]*>${localShort(Date.parse("2030-01-01T05:00:00Z"), now)}</time>`));
    doesNotMatch(five, /data-claude-stale-mark/);
    const weekly = rows.get("weekly_limit")!;
    match(weekly, new RegExp(`title="${isZh ? "完整查询" : "Full query"} · ${localShort(account.observed_at!, now, true)}"`));
    match(weekly, />12%</);
    match(weekly, isZh ? /data-claude-stale-mark[^>]*>旧</ : /data-claude-stale-mark[^>]*>Stale</);
    // Full-query windows without their own time fall back to the account observation.
    match(rows.get("feature:sonnet:weekly_limit")!, new RegExp(`title="${isZh ? "完整查询" : "Full query"} · ${localShort(account.observed_at!, now, true)}"`));
    equal((html.match(/role="progressbar"/g) ?? []).length, 3);
    doesNotMatch(html, /data-claude-quota-observation|部分观测|\(partial\)|Status|状态|Source:|来源：|Full usage snapshot|完整用量快照|已观察|Observed/);
    deepEqual(subject, original);
  }
});

test("stale and expired windows keep recorded values with one short mark, never a bar after reset or an inferred zero", (t) => {
  const now = Date.parse("2030-01-01T01:00:00Z");
  t.mock.method(Date, "now", () => now);
  for (const isZh of [false, true]) {
    for (const [observed_at, resets_at, fresh, expired] of [
      [now - 660_000, new Date(now + 1).toISOString(), true, false],
      [now - 660_001, new Date(now + 1).toISOString(), false, false],
      [now, new Date(now).toISOString(), false, true],
      [now - 660_001, new Date(now - 1).toISOString(), false, true],
      [now + 60_000, null, true, false],
      [now + 60_001, null, false, false],
      [0, null, false, false], [-1, null, false, false], [NaN, null, false, false],
      [Infinity, null, false, false], [8_640_000_000_000_001, null, false, false],
    ] as const) {
      const html = renderToStaticMarkup(<ClaudeAccountUsage account={{ ...account, stale: true, claude_usage: { windows: [
        { name: "five_hour", used_percent: 82.5, resets_at, source: "inference_header", observed_at },
      ] } }} isZh={isZh} />);
      const label = `${observed_at}/${resets_at}`;
      match(html, />82.5%</, label);
      equal(html.includes('role="progressbar"'), !expired, label);
      equal((html.match(/data-claude-stale-mark/g) ?? []).length, fresh ? 0 : 1, label);
      const valid = observed_at > 0 && observed_at <= now + 60_000;
      match(html, valid ? new RegExp(`· ${localShort(observed_at, now, true)}"`) : isZh ? /· 时间未知"/ : /· time unknown"/, label);
      doesNotMatch(html, />0%<|aria-valuenow="0"|NaN|Infinity|Invalid Date/, label);
    }
  }
});

test("invalid Claude percentages read Unknown instead of a bar, zero or a status-derived value", () => {
  for (const isZh of [false, true]) {
    for (const used_percent of [null, undefined, NaN, Infinity, -1, 101]) {
      const html = renderToStaticMarkup(<ClaudeAccountUsage account={{ ...account, claude_usage: { windows: [
        { name: "five_hour", used_percent: used_percent as unknown as number, resets_at: "2030-01-01T05:00:00Z" },
      ] } }} isZh={isZh} />);
      match(html, isZh ? />未知</ : />Unknown</);
      doesNotMatch(html, /progressbar|>0%<|>100%<|NaN|Infinity/);
    }
  }
});

test("legacy Claude card shows local effective windows compactly with a short add-accounts hint", (t) => {
  const now = Date.parse("2030-01-01T00:04:00Z");
  t.mock.method(Date, "now", () => now);
  const observed = Date.parse("2030-01-01T00:02:00Z");
  const current: ProviderAccountView = { ...account, stale: true, claude_usage: { windows: [
    { name: "five_hour", used_percent: 82.5, resets_at: "2030-01-01T05:00:00Z", source: "inference_header", observed_at: observed + 60_000 },
    { name: "weekly_limit", used_percent: 12, resets_at: "2030-01-07T00:00:00Z", source: "inference_header", observed_at: observed },
    { name: "feature:sonnet:weekly_limit", used_percent: 4, resets_at: null },
  ] } };
  const html = panel({ ...pool, config: null, accounts: [current] }, true, undefined, (client) => {
    equal(client.getQueryCache().findAll({ queryKey: ["provider-account-pool"] }).length, 1);
    const usage = client.getQueryCache().findAll({ queryKey: ["provider-usage"] });
    equal(usage.length, 1);
    equal((usage[0].options as { refetchInterval?: number }).refetchInterval, 300_000);
    equal((usage[0].options as { retry?: unknown }).retry, false);
  });
  const rows = windowRows(html);
  deepEqual([...rows.keys()], ["five_hour", "weekly_limit", "feature:sonnet:weekly_limit"]);
  match(rows.get("five_hour")!, />82.5%</);
  match(rows.get("weekly_limit")!, new RegExp(`title="From chat responses · ${localShort(observed, now, true)}"`));
  // Header time is the older main-window observation; only the model window is marked.
  match(html, /data-claude-freshness="fresh"[^>]*>[\s\S]*?<time dateTime="2030-01-01T00:02:00.000Z">2m ago<\/time>/);
  equal((html.match(/data-claude-stale-mark/g) ?? []).length, 1);
  match(rows.get("feature:sonnet:weekly_limit")!, /data-claude-stale-mark/);
  match(html, /data-claude-add-accounts-hint[^>]*>Add more accounts in Manage accounts</);
  match(html, /data-claude-add-accounts-hint="true" title="Save pool scheduling settings there to add accounts; the current account keeps working and can be re-authorized."/);
  doesNotMatch(withoutTitles(html), /Save pool scheduling settings|legacy account can still be used|data-claude-quota-observation|\(partial\)|Observed/);
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
    doesNotMatch(html, /data-claude-pool-plans|Weighted estimate|加权估算|Pro-equivalent|Pro 等价|Known usage|有效用量|Next reset|最近重置|Disabled|已停用|accounts ·|\d+ 个账号/);
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
  match(html, /<details[^>]*data-claude-pool-weights[^>]*><summary[^>]*>Weights Pro \/ Max 5× \/ Max 20×: 5h 1 \/ 5 \/ 20, weekly 1 \/ 5 \/ 10 \(estimated\)<\/summary>/);
  match(html, /Unknown or unrecognized plans count as Pro \(1\)/);
  match(html, /Known usage 3 \/ 3 accounts/);
  match(html, /not necessarily the whole pool/);
  match(html, /Plans in pool totals:/);
  match(html, /Sonnet · Weekly/);
  equal((html.match(/role="progressbar"/g) ?? []).length, 6);
  doesNotMatch(html, /Passive inference-header|do not contribute to pool totals|at least 10 minutes between/);
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
  match(unknown, /Usage unknown/);
  doesNotMatch(unknown, /progressbar|>0%</);
  const invalid = renderToStaticMarkup(<ClaudeAccountUsage account={{ ...account, observed_at: NaN, claude_usage: { windows: [{ name: "five_hour", used_percent: NaN, resets_at: "invalid" }] } }} isZh={false} />);
  match(invalid, />Unknown</); match(invalid, /data-claude-stale-mark/); match(invalid, /· time unknown"/);
  doesNotMatch(invalid, /progressbar|<time|Invalid Date|NaN|Infinity/);
  const stale = renderToStaticMarkup(<ClaudeAccountUsage account={{ ...account, stale: true, claude_usage: { windows: [{ name: "weekly_limit", used_percent: 100, resets_at: "2000-01-01T00:00:00Z" }] } }} isZh={false} />);
  match(stale, />100%</); match(stale, /data-claude-stale-mark[^>]*title="Past reset; awaiting new data"/);
  doesNotMatch(stale, /progressbar|>0%</);
});

test("Claude window names are escaped and Chinese labels are explicit", () => {
  const html = renderToStaticMarkup(<ClaudeAccountUsage account={account} isZh />);
  for (const text of ["5小时", "每周", "Sonnet · 每周", "37.5%", "完整查询"]) match(html, new RegExp(text));
  const summary = renderToStaticMarkup(<ClaudePoolSummary pool={weightedPool} isZh detailed />);
  for (const text of ["加权估算", "已用 13.5%", "已用 15.6%", "5 小时 1 / 5 / 20", "每周 1 / 5 / 10", "不一定是全池", "有效用量 3 / 3 个账号", "未知或未识别套餐按 Pro"]) match(summary, new RegExp(text));
  doesNotMatch(summary, /推理响应头|不参与账号池汇总/);
  const unsafe = renderToStaticMarkup(<ClaudeAccountUsage account={{ ...account, claude_usage: { windows: [{ name: '<script>alert(1)</script>', used_percent: 1, resets_at: null }] } }} isZh={false} />);
  doesNotMatch(unsafe, /<script>/i); match(unsafe, /&lt;Script&gt;/);
});

test("Claude management preserves retained and actual reset-cycle usage stats without Kiro units", () => {
  const zero = { total_tokens: 0, request_count: 0, equivalent_costs: [], unpriced_request_count: 0 };
  const stats: AccountUsageStats[] = [{ account_id: "account-a", all_time: { ...zero, total_tokens: 2_500_000, request_count: 1, equivalent_costs: [{ currency: "USD", total_cost: 7.5, priced_request_count: 1 }] }, period: zero, period_start: Date.now() - 86_400_000, period_end: Date.now() + 518_400_000 }];
  const html = panel(pool, false, stats);
  match(html, /data-account-usage="account-a"[\s\S]*2\.50M[\s\S]*\$7\.50[\s\S]*Current cycle[\s\S]*\$0\.00/);
  match(html, />Remove<\/button><\/div><div data-account-usage=/);
  match(html, /actual reset cycle, not a calendar week or month/);
  doesNotMatch(html, /This month|Kiro tokens|Loading totals/);
  panel(pool, true, undefined, (client) => equal(client.getQueryCache().findAll({ queryKey: ["provider-account-usage-stats"] }).length, 0));
  panel({ ...pool, accounts: [account, { ...account, account_id: "account-b" }] }, false, undefined, (client) => {
    const queries = client.getQueryCache().findAll({ queryKey: ["provider-account-usage-stats"] });
    equal(queries.length, 1);
    deepEqual(queries[0].queryKey, ["provider-account-usage-stats", "claude"]);
    equal((queries[0].options as { refetchInterval?: number }).refetchInterval, 60_000);
  });
});

test("Claude management projects full-cycle price from weekly usage, never the 5h window", () => {
  const now = Date.now();
  const end = now + 3 * 86_400_000;
  const reset = new Date(end).toISOString();
  const value = { ...pool, accounts: [{ ...account, claude_usage: { windows: [
    { name: "five_hour", used_percent: 90, resets_at: reset },
    { name: "weekly_limit", used_percent: 10, resets_at: reset },
  ] } }] };
  const totals = { total_tokens: 54_740_000, request_count: 1, unpriced_request_count: 0,
    equivalent_costs: [{ currency: "USD", total_cost: 21.4, priced_request_count: 1 }] };
  const html = panel(value, false, [{ account_id: account.account_id, all_time: totals, period: totals,
    period_start: end - 7 * 86_400_000, period_end: end }]);
  match(html, /data-account-usage-estimate[\s\S]*>Estimated capacity<[\s\S]*≈ \$214\.00/);
  match(html, /Used for projection: 10%/);
});

test("Claude login explains browser code exchange and exact target, not Devin token import", () => {
  const html = renderToStaticMarkup(<LocaleProvider><ClaudeAccountLogin intent={{ kind: "reauthorize_account", providerId: "claude", accountId: "account-a", expectedVersion: 4 }} initialLabel="Work" onBound={() => {}} onCancel={() => {}} /></LocaleProvider>);
  match(html, /Only the selected account/); match(html, /authorization code or full callback URL/);
  match(html, /Start authorization/); doesNotMatch(html, /Devin|session token|value="import"/);
});