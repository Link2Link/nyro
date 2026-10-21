import { doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { LocaleProvider } from "../lib/i18n";
import { newDevinPoolConfig, type AccountPoolView, type ProviderAccountView } from "../lib/devin-account-pool";
import type { Provider } from "../lib/types";
import { AccountPoolSettings } from "./devin-pool-settings";
import { KiroAccountPoolCard, KiroAccountPoolPanel, KiroAccountUsage, KiroPoolUsage } from "./kiro-account-pool";
import { KiroAccountLogin } from "./kiro-account-login";

const account: ProviderAccountView = {
  account_id: "account-a", provider_id: "kiro", label: "Work", is_enabled: true, priority: 2,
  status: "connected", status_version: 4, verified_plan: null, identity_verified: true,
  scheduling: "included", daily: null, weekly: null, reserve_usd: null,
  observed_at: Date.parse("2030-01-01T00:00:00Z"), stale: false, updated_at: "",
  kiro_usage: { plan: "POWER", email: "work@example.com", resets_at: "2030-02-01T00:00:00Z", overage_enabled: true, items: [
    { resource: "CREDIT", kind: "base", name: "Credits", used: 80, limit: 100, resets_at: "2030-02-01T00:00:00Z" },
    { resource: "CREDIT", kind: "bonus", name: "Welcome bonus", used: 5, limit: 50, resets_at: "2030-01-15T00:00:00Z" },
    { resource: "CHAT", kind: "free_trial", name: "Trial chats", used: 0, limit: 20, resets_at: null },
  ] },
};
const pool: AccountPoolView = {
  kind: "kiro", provider_id: "kiro", config: newDevinPoolConfig(), accounts: [account],
  quota: { daily: null, weekly: null, estimated: false }, readiness: "eligible",
  counts: { total: 1, accounting: 1, available: 1, included: 1, reserve: 0, pro: 0, max: 0, unclassified: 0,
    unknown: 0, disabled: 0, disconnected: 0, auth_required: 0, quota_exhausted: 0, plan_mismatch: 0, plan_unverified: 0 },
};
const provider: Provider = {
  id: "kiro", name: "Kiro", vendor: "kiro", protocol: "kiro-chat/generate-assistant-response/v1", auth_mode: "oauth",
  base_url: "https://q.us-east-1.amazonaws.com", use_proxy: false, fast_mode: false, is_enabled: true, created_at: "", updated_at: "",
};

function panel(value = pool, card = false) {
  const client = new QueryClient();
  client.setQueryData(["provider-account-pool", "kiro"], value);
  try {
    return renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      {card ? <KiroAccountPoolCard provider={provider} /> : <KiroAccountPoolPanel provider={provider} />}
    </LocaleProvider></QueryClientProvider>);
  } finally { client.clear(); }
}

test("Kiro management has scoped account actions and actual usage, not Devin quota rules", () => {
  const html = panel();
  for (const text of ["Kiro account pool", "Add account", "Re-authorize / import", "Refresh usage", "Refresh pool usage", "Disable", "Remove", "Account label", "Account priority", "account-a · v4", "automatically refreshes credentials", "POWER", "work@example.com", "Credits", "Welcome bonus", "Free trial", "Upstream overage enabled"]) match(html, new RegExp(text));
  match(html, /Used 80 \/ 100/);
  match(html, /dateTime="2030-01-15T00:00:00Z"/);
  // Three account allowances + two pool totals (CREDIT, CHAT).
  equal((html.match(/role="progressbar"/g) ?? []).length, 5);
  doesNotMatch(html, /Devin|Daily usage|Weekly usage|Pro-equivalent|Team Full Seat|10×|Plan unverified|Paid balance/);
});

test("shared Kiro settings retain strategy affinity attempts and explicit overage opt-in", () => {
  const html = renderToStaticMarkup(<LocaleProvider><AccountPoolSettings vendor="kiro" config={null} pending={false} onSave={() => {}} /></LocaleProvider>);
  for (const text of ["Scheduling strategy", "Round robin", "Fill first", "Session affinity TTL", "Maximum account attempts", "Enable session affinity", "Allow paid overage fallback", "requires upstream opt-in", "Save pool settings"]) match(html, new RegExp(text));
  match(html, /not a billing hard limit/);
  doesNotMatch(html, /Pro \/ Team|\bMax\b|Paid balance|value="pro"|value="max"/);
});

test("legacy Kiro login remains usable while add-account awaits pool configuration", () => {
  const html = panel({ ...pool, config: null });
  const button = html.match(/<button[^>]*>Re-authorize \/ import legacy account<\/button>/)?.[0] ?? "";
  match(button, /legacy account/);
  doesNotMatch(button, /disabled=""/);
  match(html, /<button[^>]*disabled=""[^>]*>Add account<\/button>/);
  match(html, /Save the default settings to enable the pool/);
});

test("disconnected Kiro rows retain records but disable mutations and reauthorization", () => {
  const html = panel({ ...pool, accounts: [{ ...account, status: "disconnected", scheduling: "disconnected", is_enabled: false, kiro_usage: null }] });
  for (const label of ["Refresh usage", "Re-authorize / import", "Enable", "Remove"]) {
    const button = html.match(new RegExp(`<button[^>]*>${label}</button>`))?.[0] ?? "";
    match(button, /disabled=""/);
  }
  match(html, /Usage unknown/);
  doesNotMatch(html, /role="progressbar"/);
});

test("compact Kiro card shows only the pool summary; account details stay in Manage accounts", () => {
  const html = panel(pool, true);
  match(html, /1 accounts · 1 available/);
  match(html, /aria-label="Refresh pool usage"/);
  match(html, /aria-expanded="false"[^>]*>Manage accounts/);
  // Pool totals only (CREDIT, CHAT): no per-account rows, labels or allowance bars.
  equal((html.match(/role="progressbar"/g) ?? []).length, 2);
  doesNotMatch(html, /Work · POWER|Welcome bonus|Base allowance|Observed|work@example.com|Re-authorize \/ import|Devin|Daily|Weekly|Pro-equivalent/);
});

test("unknown, zero-limit and stale Kiro usage never invent quota capacity", () => {
  const unknown = renderToStaticMarkup(<KiroAccountUsage account={{ ...account, kiro_usage: null, stale: true }} isZh={false} />);
  match(unknown, /Plan unknown|Usage unknown/);
  match(unknown, /Stale \/ unknown/);
  doesNotMatch(unknown, /role="progressbar"|Used 0/);
  const invalid = renderToStaticMarkup(<KiroAccountUsage account={{ ...account, kiro_usage: { ...account.kiro_usage!, items: [
    { resource: "zero", kind: "base", name: "Zero", used: 0, limit: 0, resets_at: "invalid-date" },
    { resource: "invalid", kind: "bonus", name: "Invalid", used: Number.NaN, limit: 10, resets_at: null },
  ] } }} isZh={false} />);
  doesNotMatch(invalid, /role="progressbar"|Invalid Date|NaN|Infinity/);
});

test("Kiro resource usage includes Chinese translations without Devin terminology", () => {
  const html = renderToStaticMarkup(<KiroAccountUsage account={account} isZh />);
  for (const text of ["基础额度", "赠送额度", "免费试用", "资源", "重置 / 到期", "上游超额计费已开启", "已观察"]) match(html, new RegExp(text));
  doesNotMatch(html, /每日|每周|Pro 等价|套餐未验证/);
});

test("Kiro reserve scheduling is paid overage rather than a Devin USD balance", () => {
  const value = { ...pool, readiness: "reserve_only", accounts: [{ ...account, scheduling: "reserve" }] };
  for (const card of [false, true]) {
    const html = panel(value, card);
    match(html, /Paid overage only/);
    doesNotMatch(html, /Paid balance|\$|Pro-equivalent/);
  }
  match(panel(value), /Paid overage</);
});

test("expired Kiro bonus and trial snapshots are historical in compact and full views", () => {
  const previousNow = Date.now;
  Date.now = () => Date.parse("2030-01-20T00:00:00Z");
  try {
    const historical = { ...account, kiro_usage: { ...account.kiro_usage!, items: [
      { ...account.kiro_usage!.items[0], resets_at: "2030-01-01T00:00:00Z" },
      account.kiro_usage!.items[1],
      { ...account.kiro_usage!.items[2], resets_at: "2030-01-19T00:00:00Z" },
    ] } };
    for (const compact of [false, true]) {
      const html = renderToStaticMarkup(<KiroAccountUsage account={historical} isZh={false} compact={compact} />);
      equal((html.match(/Expired \(historical usage\)/g) ?? []).length, 2);
      // The base reset does not revive or expire an account; it stays a snapshot.
      equal((html.match(/role="progressbar"/g) ?? []).length, 1);
      doesNotMatch(html, /aria-label="(Welcome bonus|Trial chats)/);
      match(html, /Used 5 \/ 50/);
      const chinese = renderToStaticMarkup(<KiroAccountUsage account={historical} isZh compact={compact} />);
      match(chinese, /已到期（历史用量）/);
    }
  } finally { Date.now = previousNow; }
});

test("untrusted Kiro plan email and allowance names stay escaped text", () => {
  const html = renderToStaticMarkup(<KiroAccountUsage account={{ ...account, kiro_usage: {
    ...account.kiro_usage!, plan: "<script>plan</script>", email: "<b>email</b>", items: [{ ...account.kiro_usage!.items[0], name: '<img src=x onerror="alert(1)">' }],
  } }} isZh={false} />);
  match(html, /&lt;script&gt;plan/);
  match(html, /&lt;b&gt;email/);
  match(html, /&lt;img/);
  doesNotMatch(html, /<script>|<img|<b>email/);
});

test("Kiro account login keeps Google GitHub Builder ID IAM and import options", () => {
  const html = renderToStaticMarkup(<LocaleProvider><KiroAccountLogin intent={{ kind: "reauthorize_account", providerId: "kiro", accountId: "account-a", expectedVersion: 4 }} initialLabel="Work" onBound={() => {}} onCancel={() => {}} /></LocaleProvider>);
  for (const method of ["google", "github", "builder-id", "idc", "import"]) match(html, new RegExp(`value="${method}"`));
  match(html, /Only the selected account/);
  match(html, /Start authorization/);
  match(html, /gateway checks the result automatically/);
  doesNotMatch(html, /<textarea|type="password"|Devin/);
});

function withNow<T>(iso: string, run: () => T): T {
  const previousNow = Date.now;
  Date.now = () => Date.parse(iso);
  try { return run(); } finally { Date.now = previousNow; }
}

const monthly = (id: string, label: string, plan: string, used: number, limit: number): ProviderAccountView => ({
  ...account, account_id: id, label,
  kiro_usage: { plan, email: null, resets_at: "2026-11-01T00:00:00Z", overage_enabled: false, items: [
    { resource: "CREDIT", kind: "base", name: "Credit", used, limit, resets_at: "2026-11-01T00:00:00Z" },
  ] },
});
const twoAccounts: AccountPoolView = { ...pool, accounts: [monthly("a", "Kiro", "PRO+", 1153.6, 2000), monthly("b", "Kiro 2", "PRO MAX", 0.05, 5000)],
  counts: { ...pool.counts, total: 2, accounting: 2, available: 2, included: 2 } };

test("Kiro pool card shows the summed pool percent, totals and a weighted steady-pace marker", () => {
  const html = withNow("2026-10-21T10:00:00Z", () => panel(twoAccounts, true));
  // The card carries the pool total only; per-account bars live in Manage accounts.
  equal((html.match(/role="progressbar"/g) ?? []).length, 1);
  doesNotMatch(html, /Kiro 2 · PRO MAX|Used 0\.05 \/ 5,000|Observed/);
  match(html, /Monthly · Pool total/);
  match(html, />16%</);
  match(html, /Used 1,153\.65 \/ 7,000/);
  match(html, /Remaining 83\.5%/);
  match(html, /Steady-pace estimate: 64\.7%/);
  match(html, /Usage coverage: 2\/2 accounts/);
  match(html, /data-steady-pace-marker="weighted"[^>]*title="Steady pace: 64\.7% \(weighted by each account&#x27;s monthly window progress and allowance; estimate only\)"/);
  match(html, /10d14h/);
  doesNotMatch(html, /Pro-equivalent|Daily|Weekly|NaN/);
  const zh = renderToStaticMarkup(<KiroPoolUsageZh />);
  for (const text of ["每月 · 账号池合计", "已用 1,153.65 / 7,000", "剩余 83.5%", "匀速使用预估：64.7%", "用量覆盖: 2/2 账号"]) match(zh, new RegExp(text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
});

function KiroPoolUsageZh() {
  return <KiroPoolUsage pool={twoAccounts} isZh now={Date.parse("2026-10-21T10:00:00Z")} detailed />;
}

test("Kiro pool panel explains aligned resets; unknown members lower coverage without inventing usage", () => {
  const html = withNow("2026-10-21T10:00:00Z", () => panel(twoAccounts));
  match(html, /Aligned reset: <time dateTime="2026-11-01T00:00:00.000Z">/);
  match(html, /\(2\/2 accounts\)/);
  const partial = renderToStaticMarkup(<KiroPoolUsage pool={{ ...twoAccounts, accounts: [twoAccounts.accounts[0], { ...twoAccounts.accounts[1], kiro_usage: null }] }}
    isZh={false} now={Date.parse("2026-10-21T10:00:00Z")} />);
  match(partial, /Used 1,153\.6 \/ 2,000/);
  match(partial, /Usage coverage: 1\/2 accounts/);
  const none = renderToStaticMarkup(<KiroPoolUsage pool={{ ...twoAccounts, accounts: twoAccounts.accounts.map((a) => ({ ...a, kiro_usage: null })) }}
    isZh={false} now={Date.parse("2026-10-21T10:00:00Z")} />);
  match(none, /Pool total: usage unknown/);
  doesNotMatch(none, /role="progressbar"|0%/);
});

test("credential-shaped account labels are flagged and cannot be saved or used to start login", () => {
  // Synthetic fixture shaped like a pasted credential file; not a real token.
  const leaked = '"refreshToken": "aorAAAAAFakeFixtureTokenNotReal0000000000';
  const html = panel({ ...pool, accounts: [{ ...account, label: leaked }] });
  match(html, /This label looks like a credential; use a plain name/);
  const login = renderToStaticMarkup(<LocaleProvider><KiroAccountLogin intent={{ kind: "add_account", providerId: "kiro" }} initialLabel={leaked} onBound={() => {}} onCancel={() => {}} /></LocaleProvider>);
  match(login, /looks like a credential/);
  match(login, /<button[^>]*disabled=""[^>]*>Start authorization<\/button>/);
  const clean = renderToStaticMarkup(<LocaleProvider><KiroAccountLogin intent={{ kind: "add_account", providerId: "kiro" }} initialLabel="Kiro 2" onBound={() => {}} onCancel={() => {}} /></LocaleProvider>);
  doesNotMatch(clean, /looks like a credential/);
});

test("Kiro card puts the pool refresh as a Devin-style top-right icon inside the usage box", () => {
  const html = panel(pool, true);
  // One box: refresh icon first, then counts and pool totals inside it.
  match(html, /<div class="relative space-y-2 rounded-lg[^"]*pr-10"><button[^>]*aria-label="Refresh pool usage"[^>]*aria-busy="false"[^>]*class="absolute right-2 top-1\.5[^"]*"><svg[^>]*aria-hidden="true"[^>]*>[\s\S]*?<\/svg><\/button><div[^>]*><p>1 accounts · 1 available/);
  match(html, /CREDIT · Total pool</);
  doesNotMatch(html, /Total pool · Pool total/);
  doesNotMatch(html, />Refresh pool usage</);
  const pending = renderToStaticMarkup(<KiroPoolUsage pool={pool} isZh={false} now={Date.now()} counts onRefresh={() => {}} refreshing />);
  match(pending, /<button[^>]*disabled=""[^>]*aria-label="Refresh pool usage"[^>]*aria-busy="true"/);
  match(pending, /Refreshing usage…/);
  match(pending, /animate-spin/);
  // Without a refresh handler (management panel), no icon is rendered.
  doesNotMatch(renderToStaticMarkup(<KiroPoolUsage pool={pool} isZh={false} now={Date.now()} detailed />), /aria-label="Refresh pool usage"/);
});
