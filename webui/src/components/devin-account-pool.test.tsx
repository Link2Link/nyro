import { doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { LocaleProvider } from "../lib/i18n";
import { newDevinPoolConfig, type AccountPoolView, type PoolQuotaWindow, type ProviderAccountView } from "../lib/devin-account-pool";
import type { Provider } from "../lib/types";
import { DevinPoolUsage } from "./provider-usage";
import { DevinPoolSettings } from "./devin-pool-settings";
import { DevinAccountPoolCard, DevinAccountPoolPanel } from "./devin-account-pool";

const weekly: PoolQuotaWindow = {
  total_units: 12, total_accounts: 3, capacity_complete: true, known_accounts: 3,
  known_remaining_units: 5.8, used_units: 6.2, remaining_units: 5.8,
  used_percent: 51.6667, remaining_percent: 48.3333, stale: false,
  next_reset_at: "2030-01-01T01:00:00Z", next_reset_accounts: 1, reset_mode: "staggered",
};
const mixed: AccountPoolView = {
  provider_id: "devin", config: newDevinPoolConfig(), accounts: [],
  quota: {
    daily: { ...weekly, total_units: 2, total_accounts: 2, known_accounts: 2, known_remaining_units: 1.8,
      used_units: 0.2, remaining_units: 1.8, used_percent: 10, remaining_percent: 90 },
    weekly, estimated: true,
  },
  counts: { total: 3, pro: 2, max: 1, unclassified: 0, accounting: 3, available: 3, included: 3,
    reserve: 0, unknown: 0, disabled: 0, disconnected: 0, auth_required: 0,
    quota_exhausted: 0, plan_mismatch: 0, plan_unverified: 0 },
  readiness: "eligible",
};

function usage(pool: AccountPoolView, detailed = true): string {
  return renderToStaticMarkup(<LocaleProvider><DevinPoolUsage pool={pool} detailed={detailed} /></LocaleProvider>);
}

function panel(pool: AccountPoolView, collapsed = false): string {
  const client = new QueryClient();
  client.setQueryData(["provider-account-pool", "devin"], pool);
  const provider: Provider = {
    id: "devin", name: "Devin", vendor: "devin", protocol: "devin-chat/get-chat-message/v1",
    base_url: "https://api.devin.ai", use_proxy: false, fast_mode: false, is_enabled: true,
    created_at: "", updated_at: "",
  };
  try {
    return renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      {collapsed ? <DevinAccountPoolCard provider={provider} /> : <DevinAccountPoolPanel provider={provider} />}
    </LocaleProvider></QueryClientProvider>);
  } finally {
    client.clear();
  }
}

test("collapsed card shows only quota rows and keeps pool details inside account management", () => {
  const pool: AccountPoolView = { ...mixed, counts: { ...mixed.counts, disabled: 1 }, accounts: [
    paceAccount("a", "pro", 60, 20, 25, 100),
    paceAccount("b", "pro", 30, 60, 75, 0),
    paceAccount("c", "max", 0, 50, 0, 0),
  ] };
  frozenNow(() => {
    for (const html of [usage(pool, false), panel(pool, true)]) {
      equal((html.match(/role="progressbar"/g) ?? []).length, 2);
      equal((html.match(/data-steady-pace-marker="weighted"/g) ?? []).length, 2);
      match(html, />Daily</);
      match(html, />Weekly</);
      match(html, /Used 51\.7%/);
      match(html, /Remaining 48\.3%/);
      match(html, /5\.80 \/ 12 Pro-equivalent units/);
      doesNotMatch(html, /<p\b|<strong>Devin|>Mixed<|3 accounts ·|Estimated capacity|Pro \/ Team Full Seat|Steady-pace estimate|Usage coverage|Next account reset|Excluded from capacity/);
    }
    match(panel(pool, true), /aria-expanded="false"[^>]*>Manage accounts/);
    const expanded = panel(pool);
    match(expanded, /3 accounts · 3 available/);
    match(expanded, /Estimated capacity/);
    match(expanded, /Pro \/ Team Full Seat: 2 · Max: 1/);
    match(expanded, /Steady-pace estimate: 50\.0%/);
    match(expanded, /Usage coverage: 3\/3 accounts/);
    match(expanded, /Next account reset/);
    match(expanded, /Excluded from capacity/);
  });
});

test("collapsed pool card exposes a refresh action without opening account management", () => {
  const html = panel(mixed, true);
  const button = html.match(/<button[^>]*aria-label="Refresh pool usage"[^>]*>/)?.[0] ?? "";
  match(button, /type="button"/);
  match(button, /aria-busy="false"/);
  doesNotMatch(button, /disabled=""/);
  match(html, /aria-expanded="false"[^>]*>Manage accounts/);
  doesNotMatch(html, /Devin account pool|Usage coverage/);
  equal((html.match(/role="progressbar"/g) ?? []).length, 2);
});

test("pool refresh action indicates pending state and prevents duplicate clicks", () => {
  const html = renderToStaticMarkup(<LocaleProvider>
    <DevinPoolUsage pool={mixed} onRefresh={() => {}} refreshing />
  </LocaleProvider>);
  match(html, /<button[^>]*disabled=""[^>]*aria-label="Refresh pool usage"[^>]*aria-busy="true"/);
  match(html, /Refreshing usage…/);
  match(html, /animate-spin/);
  equal((html.match(/role="progressbar"/g) ?? []).length, 2);
  doesNotMatch(usage(mixed), /aria-label="Refresh pool usage"/);
});

test("compact quota rows retain stale and incomplete usage warnings", () => {
  const html = usage({ ...mixed, quota: { ...mixed.quota, weekly: {
    ...weekly, stale: true, used_percent: null, remaining_percent: null, remaining_units: null,
  } } }, false);
  match(html, /Updating usage/);
  match(html, /Stale \/ incomplete/);
  doesNotMatch(html, /Used 51\.7%|Usage coverage/);
});

test("mixed quota displays backend weighted capacity, coverage and account resets", () => {
  const html = usage(mixed);
  match(html, />Mixed</);
  match(html, /Pro \/ Team Full Seat: 2 · Max: 1/);
  match(html, /5\.80 \/ 12 Pro-equivalent units/);
  match(html, /1\.80 \/ 2 Pro-equivalent units/);
  match(html, /Used 51\.7%/);
  match(html, /Remaining 48\.3%/);
  match(html, /Usage coverage: 3\/3 accounts/);
  match(html, /\(1\/3 accounts\)/);
  match(html, /Non-Max accounts only; Max has no daily quota/);
  doesNotMatch(html, /3\/12 accounts|1\/12 accounts|account units/);
});

test("Max hides backend-null daily quota and table columns regardless of legacy config", () => {
  const max: AccountPoolView = {
    ...mixed, config: { ...newDevinPoolConfig(), plan: "pro" },
    counts: { ...mixed.counts, pro: 0, max: 3 }, quota: { ...mixed.quota, daily: null },
  };
  const html = usage(max);
  match(html, />Max</);
  doesNotMatch(html, /Daily|daily quota|Mixed/);
  doesNotMatch(panel(max), /Daily usage|Daily remaining/);
  // Conversely, a legacy Max value must not hide a backend daily snapshot.
  match(usage({ ...mixed, config: { ...newDevinPoolConfig(), plan: "max" } }), /Daily/);
});

test("unclassified types suppress full percentages without claiming zero capacity", () => {
  const pending: AccountPoolView = {
    ...mixed, counts: { ...mixed.counts, pro: 0, max: 0, unclassified: 3, plan_unverified: 3 },
    quota: { ...mixed.quota, daily: null, weekly: { ...weekly, total_units: 0, capacity_complete: false } },
  };
  const html = usage(pending);
  match(html, /Type pending: 3/);
  match(html, /Account types pending; capacity incomplete/);
  match(html, /Usage coverage: 3\/3 accounts/);
  doesNotMatch(html, /role="progressbar"|Remaining .*%|Used .*%|No eligible pool capacity|>Pro<|>Max<|>Mixed<|plan mismatch|Capacity exclusions|Excluded from capacity/);
  const partialUsage = usage({ ...mixed, quota: { ...mixed.quota, daily: null, weekly: {
    ...weekly, known_accounts: 2, remaining_units: null, remaining_percent: null, used_percent: null,
  } } });
  match(partialUsage, /Updating usage/);
  match(partialUsage, /Usage coverage: 2\/3 accounts/);
  doesNotMatch(partialUsage, /2\/12 accounts|role="progressbar"/);
});

test("Pro badge trusts backend counts even with no frontend account rows", () => {
  const html = usage({ ...mixed, counts: { ...mixed.counts, pro: 3, max: 0 } });
  match(html, />Pro</);
  doesNotMatch(html, />Mixed<|>Max</);
});

test("pool settings enable defaults without plan selection and do not lock strategy", () => {
  for (const config of [null, { ...newDevinPoolConfig(), plan: "max" as const }]) {
    const html = renderToStaticMarkup(<LocaleProvider>
      <DevinPoolSettings config={config} pending={false} onSave={() => {}} />
    </LocaleProvider>);
    match(html, /Account types are detected automatically/);
    match(html, /value="round_robin" selected=""/);
    match(html, /Save pool settings/);
    doesNotMatch(html, /disabled=""|Plan \(required\)|Select a plan|value="pro"|value="max"/);
  }
});

const paceNow = Date.parse("2030-01-01T00:00:00Z");
function paceAccount(id: string, plan: "pro" | "max", dailyRemaining: number, weeklyRemaining: number, dailyPace: number, weeklyPace: number): ProviderAccountView {
  const reset = (days: number, pace: number) => new Date(paceNow + days * 86400_000 * (1 - pace / 100)).toISOString();
  return {
    account_id: id, provider_id: "devin", label: id, is_enabled: true, priority: 0,
    status: "connected", status_version: 1, verified_plan: plan, identity_verified: false,
    scheduling: "included", daily: plan === "max" ? null : { remaining_percent: dailyRemaining, resets_at: reset(1, dailyPace) },
    weekly: { remaining_percent: weeklyRemaining, resets_at: reset(7, weeklyPace) },
    reserve_usd: 0, observed_at: paceNow, stale: false, updated_at: "",
  };
}
function frozenNow<T>(run: () => T): T {
  const original = Date.now;
  Date.now = () => paceNow;
  try { return run(); } finally { Date.now = original; }
}

test("account rows use the shared used-percent track and consistent utilization colors", () => {
  const pool: AccountPoolView = { ...mixed, accounts: [
    paceAccount("green", "pro", 83, 91, 25, 25),
    paceAccount("orange", "pro", 25, 5, 75, 75),
    paceAccount("max", "max", 0, 100, 0, 50),
  ] };
  const html = frozenNow(() => panel(pool));
  match(html, />Daily usage</);
  match(html, />Weekly usage</);
  match(html, /Used 17\.0%/);
  match(html, /Remaining 83\.0%/);
  match(html, /aria-label="Account daily used quota"[^>]*aria-valuenow="17"/);
  match(html, /bg-emerald-500[^>]*style="width:17%"/);
  match(html, /bg-orange-500[^>]*style="width:75%"/);
  match(html, /bg-red-500[^>]*style="width:95%"/);
  match(html, /Used 0\.0%/);
  match(html, /Not applicable/);
  equal((html.match(/data-steady-pace-marker="account"/g) ?? []).length, 5);
});

test("pool needles use staggered account reset progress weighted by Pro-equivalent capacity", () => {
  const pool: AccountPoolView = { ...mixed, accounts: [
    paceAccount("a", "pro", 60, 20, 25, 100),
    paceAccount("b", "pro", 30, 60, 75, 0),
    paceAccount("c", "max", 0, 50, 0, 0),
  ] };
  const html = frozenNow(() => usage(pool));
  equal((html.match(/data-steady-pace-marker="weighted"/g) ?? []).length, 2);
  match(html, /left:50%/);
  match(html, /Steady-pace estimate: 50\.0%/);
  match(html, /Steady-pace estimate: 8\.3%/);
  match(html, /weighted by each account&#x27;s window progress and Pro-equivalent capacity/);
  match(html, /Used 51\.7%/); // Backend actual usage remains unchanged.
  const missingReset = { ...pool, accounts: pool.accounts.map(a => a.account_id === "a" ? { ...a, daily: { ...a.daily!, resets_at: null }, weekly: { ...a.weekly!, resets_at: null } } : a) };
  doesNotMatch(frozenNow(() => usage(missingReset)), /data-steady-pace-marker|Steady-pace estimate/);
});

test("missing account usage is not rendered as an empty zero-used allowance", () => {
  const account = { ...paceAccount("unknown", "pro", 60, 20, 25, 25), daily: null, weekly: null };
  const html = frozenNow(() => panel({ ...mixed, accounts: [account] }));
  match(html, /Unknown/);
  doesNotMatch(html, /aria-label="Account (daily|weekly) used quota"/);
});

test("legacy reauthorization remains available before enabling the pool", () => {
  const html = panel({ ...mixed, config: null, accounts: [{
    account_id: "legacy", provider_id: "devin", label: "Existing", is_enabled: true, priority: 0,
    status: "connected", status_version: 1, verified_plan: "pro", identity_verified: true,
    scheduling: "included", daily: null, weekly: null, reserve_usd: null, observed_at: null,
    stale: true, updated_at: "",
  }] });
  match(html, /PRO · Identity verified/);
  const legacyButton = html.match(/<button[^>]*>Re-authorize legacy account<\/button>/)?.[0] ?? "";
  match(legacyButton, /Re-authorize legacy account/);
  doesNotMatch(legacyButton, /disabled=""/);
  match(html, /<button[^>]*disabled=""[^>]*>Add account<\/button>/);
  match(html, /Save the default scheduling settings to enable the pool/);
  doesNotMatch(html, /Select and verify|Plan \(required\)|value="pro"|value="max"/);
});
