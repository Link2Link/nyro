import { doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { LocaleProvider } from "../lib/i18n";
import type { Provider, ProviderUsage } from "../lib/types";
import { ProviderUsageFooter } from "./provider-usage";

const now = Date.parse("2030-01-01T00:00:00Z");
const provider: Provider = {
  id: "usage", name: "Usage", vendor: "zai", protocol: "openai-compatible",
  base_url: "https://api.z.ai", use_proxy: false, fast_mode: false, is_enabled: true,
  created_at: "", updated_at: "",
};
const base: ProviderUsage = {
  provider_id: provider.id, kind: "glm_coding_plan", site: "global", tiers: [], balances: [], spends: [],
  is_available: true, queried_at: new Date(now).toISOString(),
};

function render(usage: ProviderUsage, error?: Error): string {
  const client = new QueryClient();
  const key = ["provider-usage", provider.id];
  client.setQueryData(key, usage);
  if (error) client.getQueryCache().find({ queryKey: key })!.setState({ status: "error", error });
  const originalNow = Date.now;
  Date.now = () => now;
  try {
    return renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      <ProviderUsageFooter provider={provider} />
    </LocaleProvider></QueryClientProvider>);
  } finally {
    Date.now = originalNow;
    client.clear();
  }
}

test("provider usage always renders every full tier and pace marker without disclosure", () => {
  const html = render({ ...base, tiers: [
    { name: "five_hour", used_percent: 27, resets_at: new Date(now + 2.5 * 3600_000).toISOString() },
    { name: "weekly_limit", used_percent: 19, resets_at: new Date(now + 3.5 * 86400_000).toISOString() },
    { name: "monthly", used_percent: 32, resets_at: new Date(now + 15 * 86400_000).toISOString() },
  ] });
  equal((html.match(/role="progressbar"/g) ?? []).length, 3);
  equal((html.match(/data-steady-pace-marker/g) ?? []).length, 3);
  match(html, /Steady pace: 50%/);
  match(html, /27%/);
  match(html, /19%/);
  match(html, /32%/);
  match(html, /Refresh usage/);
  doesNotMatch(html, /<details|<summary|Show details|Hide details|展开详情|收起详情/);
});

test("balance and spend details stay visible without an expand action", () => {
  const html = render({ ...base, kind: "deepseek_balance", balances: [
    { currency: "CNY", total: 56.14, granted: 1, topped_up: 55.14 },
  ], spends: [{ name: "today", currency: "CNY", amount: 2.5 }] });
  match(html, /56\.14/);
  match(html, /55\.14/);
  match(html, /2\.50/);
  doesNotMatch(html, /<details|<summary|Show details|Hide details/);
  match(render({ ...base, spends: [{ name: "today", currency: "USD", amount: 1.25 }] }), /1\.25/);
});

test("native pool and single-account authentication pauses never claim exhausted quota or recovery", () => {
  for (const kind of ["devin_plan", "kiro_plan", "glm_coding_plan"]) {
    for (const reason of ["auth_required", "account_unavailable", "no_account", "disabled", "plan_verification_required", "future_reason"]) {
      const html = render({ ...base, kind, is_available: false,
        scheduling: { status: "quota_exhausted", reason, blocking_tiers: ["monthly"],
          reset_at: new Date(now + 3600_000).toISOString(), next_check_at: new Date(now + 300_000).toISOString() },
      });
      match(html, /routing paused/);
      doesNotMatch(html, /Quota exhausted|Quota reset|Monthly|1h|5m|ranked last/);
      if (reason === "auth_required") match(html, /Needs login, routing paused/);
    }
  }
});

test("Devin and Kiro pool readiness explains a generic pause even for legacy singleton config", () => {
  for (const kind of ["devin_plan", "kiro_plan"]) {
    const html = render({ ...base, kind,
      scheduling: { status: "quota_exhausted", reason: "account_unavailable", blocking_tiers: [],
        next_check_at: new Date(now + 300_000).toISOString() },
      account_pool: {
        provider_id: provider.id, config: null, accounts: [], readiness: "auth_required",
        quota: { daily: null, weekly: null, estimated: false },
        counts: { total: 1, accounting: 1, available: 0, included: 0, reserve: 0,
          pro: 0, max: 0, unclassified: 1, unknown: 0, disabled: 0, disconnected: 0,
          auth_required: 1, quota_exhausted: 0, plan_mismatch: 0, plan_unverified: 0 },
      },
    });
    match(html, /Needs login, routing paused/);
    doesNotMatch(html, /Quota exhausted|Quota reset|5m/);
  }
});

test("quota pause shows only a labeled actual reset, not the polling deadline", () => {
  const scheduling = { status: "quota_exhausted" as const, reason: "usage_limit", blocking_tiers: ["monthly"],
    next_check_at: new Date(now + 300_000).toISOString() };
  const withoutReset = render({ ...base, scheduling });
  match(withoutReset, /Quota exhausted, routing paused.*Monthly/);
  doesNotMatch(withoutReset, /Quota reset|5m/);
  const withReset = render({ ...base, scheduling: { ...scheduling, reset_at: new Date(now + 3600_000).toISOString() } });
  match(withReset, /Quota reset: 1h/);
  doesNotMatch(withReset, /5m/);
});

test("eligible Devin reserve and Kiro overage remain available without pause warnings", () => {
  for (const kind of ["devin_plan", "kiro_plan"]) {
    const html = render({ ...base, kind, tiers: [{ name: "monthly", used_percent: 100 }],
      balances: [{ currency: "USD", total: 5, granted: 0, topped_up: 5 }],
      scheduling: { status: "eligible", blocking_tiers: [] },
    });
    match(html, /ranked last/);
    doesNotMatch(html, /routing paused|Quota reset/);
  }
});

test("cached usage retains visible refresh failures, stale state and quota pause warning", () => {
  const html = render({ ...base, tiers: [{ name: "weekly_limit", used_percent: 100 }], scheduling: {
    status: "quota_exhausted", blocking_tiers: ["weekly_limit"], reason: "usage_limit",
  } }, new Error("Temporary status failure"));
  match(html, /Quota exhausted, routing paused/);
  match(html, /Usage query failed: Temporary status failure/);
  match(html, /Stale/);
  match(html, /aria-valuenow="100"/);
  doesNotMatch(html, /<details|<summary/);
});

const upstreamUsage = (over: Partial<ProviderUsage>): ProviderUsage => ({ ...base, provider_id: "remote", ...over });

test("a chained Nyro lists forwarded upstream providers, most urgent first, with their routes", () => {
  const html = render({ ...base, kind: "nyro_gateway", upstream: { redacted: ["balances", "spends"], providers: [
    { id: "a", name: "Kimi Coding", vendor: "kimi-code", routes: ["kimi", "claude-sonnet"], status: "ok",
      usage: upstreamUsage({ kind: "kimi_coding_plan", tiers: [{ name: "five_hour", used_percent: 41 }] }) },
    { id: "b", name: "GLM", vendor: "zhipuai", routes: ["glm"], status: "ok", usage: upstreamUsage({
      level: "max", tiers: [{ name: "weekly_limit", used_percent: 100 }],
      scheduling: { status: "quota_exhausted", reason: "usage_limit", blocking_tiers: ["weekly_limit"],
        reset_at: new Date(now + 3600_000).toISOString() } }) },
    { id: "c", name: "Broken", routes: ["x"], status: "error", error: "HTTP 500" },
    { id: "d", name: "Relay", routes: ["relay"], status: "unsupported" },
  ] } });
  match(html, /Upstream Nyro Usage/);
  match(html, /4 upstream providers/);
  match(html, /1 paused/);
  match(html, /1 query failed/);
  // Paused GLM is listed before the 41% Kimi row; failures come after both.
  match(html, /GLM.*Paused.*1h.*Kimi Coding.*Broken/s);
  match(html, /Routes: kimi, claude-sonnet/);
  match(html, /Upstream query failed: HTTP 500/);
  match(html, /No usage data: Relay/);
  match(html, /visible only to the remote&#x27;s gateway key/);
  equal((html.match(/role="progressbar"/g) ?? []).length, 2);
  doesNotMatch(html, /No usage data<\/p>/);
});

test("a chained Nyro collapses long upstream lists and explains an empty route scope", () => {
  const providers = Array.from({ length: 6 }, (_, i) => ({
    id: `p${i}`, name: `Provider ${i}`, routes: [`r${i}`], status: "ok" as const,
    usage: upstreamUsage({ tiers: [{ name: "daily", used_percent: 10 * i }] }),
  }));
  const html = render({ ...base, kind: "nyro_gateway", upstream: { providers } });
  match(html, /Show 2 more/);
  equal((html.match(/role="progressbar"/g) ?? []).length, 4);
  doesNotMatch(html, /Provider 0</);
  match(render({ ...base, kind: "nyro_gateway", upstream: { providers: [] } }), /This key has no routes on the remote/);
});

test("all upstream providers paused pauses the chained Nyro with the first recovery time", () => {
  const html = render({ ...base, kind: "nyro_gateway", upstream: { providers: [] },
    scheduling: { status: "quota_exhausted", reason: "usage_limit", blocking_tiers: ["nyro_upstream"],
      reset_at: new Date(now + 2 * 3600_000).toISOString() } });
  match(html, /Quota exhausted, routing paused · All upstream providers paused/);
  match(html, /Quota reset: 2h/);
});
