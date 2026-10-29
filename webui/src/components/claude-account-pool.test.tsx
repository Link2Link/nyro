import { doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { LocaleProvider } from "../lib/i18n";
import { newDevinPoolConfig, type AccountPoolView, type ProviderAccountView } from "../lib/devin-account-pool";
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

test("Claude summary never synthesizes pooled percentages from different accounts", () => {
  const html = panel({ ...pool, accounts: [account, { ...account, account_id: "account-b", claude_usage: { windows: [{ name: "five_hour", used_percent: 90, resets_at: null }] } }] }, true);
  match(html, /percentages across plans are not summed or averaged/);
  match(html, /aria-label="Refresh pool usage"/);
  match(html, /aria-expanded="false"[^>]*>Manage accounts/);
  doesNotMatch(html, /role="progressbar"|Used [\d.]+%|Pro-equivalent/);
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
  const summary = renderToStaticMarkup(<ClaudePoolSummary pool={pool} isZh />);
  match(summary, /不可相加或平均/);
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
