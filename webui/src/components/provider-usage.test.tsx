import { doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { LocaleProvider } from "../lib/i18n";
import type { Provider, ProviderApiPriceStats, ProviderUsage } from "../lib/types";
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

function render(usage: ProviderUsage, error?: Error, currentProvider: Provider = provider): string {
  const originalNow = Date.now;
  Date.now = () => now;
  const client = new QueryClient();
  const key = ["provider-usage", currentProvider.id];
  client.setQueryData(key, usage);
  if (error) client.getQueryCache().find({ queryKey: key })!.setState({ status: "error", error });
  try {
    return renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      <ProviderUsageFooter provider={currentProvider} />
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

const apiPrice: ProviderApiPriceStats = {
  all_time: { total_tokens: 1_000_000, request_count: 2, equivalent_costs: [{ currency: "USD", total_cost: 30, priced_request_count: 2 }], unpriced_request_count: 0 },
  period: { total_tokens: 100_000, request_count: 1, equivalent_costs: [{ currency: "USD", total_cost: 3, priced_request_count: 1 }], unpriced_request_count: 0 },
  period_start: now - 3 * 86400_000, period_end: now + 4 * 86400_000, period_tier: "weekly_limit", used_percent: 10,
  estimated_capacity: [{ currency: "USD", total_cost: 30, priced_request_count: 1 }],
};
const priceUsage: ProviderUsage = { ...base, api_price: apiPrice,
  tiers: [{ name: "weekly_limit", used_percent: 10, resets_at: new Date(apiPrice.period_end!).toISOString() }] };
const priceValues = (html: string) => [...html.matchAll(/<dd[^>]*>(.*?)<\/dd>/g)].map((result) => result[1]).join(",");
const googleProviders = ["antigravity", "gemini-cli"].map((channel): Provider => ({
  ...provider, id: `google-${channel}`, name: `Google AI Pro ${channel}`, vendor: "google", preset_key: "google",
  channel, auth_mode: "oauth", protocol: "gemini", base_url: "https://cloudcode-pa.googleapis.com",
}));
const googleUsage = (currentProvider: Provider): ProviderUsage => ({
  ...priceUsage, provider_id: currentProvider.id, kind: "google_subscription", level: "G1-PRO-TIER",
  tiers: [{ name: "five_hour", used_percent: 50, resets_at: new Date(now + 2.5 * 3600_000).toISOString() },
    ...priceUsage.tiers],
});

test("GLM MiniMax xAI OpenAI Codex and Google subscription footers include all three API prices under the quota bars", () => {
  for (const kind of ["glm_coding_plan", "minimax_coding_plan", "grok_plan", "openai_codex", "google_subscription"]) {
    const html = render({ ...priceUsage, kind });
    match(html, /Total API price/);
    match(html, /Current-cycle API price/);
    match(html, /Estimated cycle API capacity/);
    match(html, /data-api-price="all_time"[\s\S]*>\$30\.00<[^]*data-api-price="period"[\s\S]*>\$3\.00<[^]*>≈ \$30\.00</);
    equal((html.match(/role="progressbar"/g) ?? []).length, 1);
    doesNotMatch(html, /<details|<summary|Show details/);
  }
  for (const kind of ["deepseek_balance", "openai", "devin_plan", "kiro_plan", "claude_code_plan", "nyro_gateway"]) {
    doesNotMatch(render({ ...priceUsage, kind }), /data-provider-api-price/);
  }
});

test("OpenAI Pro OAuth card displays weekly prices and hides only the estimate on stale or expired data", () => {
  const codexProvider: Provider = { ...provider, id: "openai-pro", name: "OpenAI Pro 200", vendor: "openai", preset_key: "openai",
    channel: "codex", auth_mode: "oauth", base_url: "https://chatgpt.com/backend-api/codex" };
  const codexUsage: ProviderUsage = { ...priceUsage, provider_id: codexProvider.id, kind: "openai_codex", level: "pro",
    tiers: [{ name: "weekly_limit", used_percent: 76, resets_at: new Date(apiPrice.period_end!).toISOString() }],
    api_price: { ...apiPrice, used_percent: 76,
      period: { ...apiPrice.period!, equivalent_costs: [{ currency: "USD", total_cost: 152, priced_request_count: 1 }] },
      estimated_capacity: [{ currency: "USD", total_cost: 200, priced_request_count: 1 }],
    },
  };
  const html = render(codexUsage, undefined, codexProvider);
  match(html, /ChatGPT Codex Usage/);
  match(html, /class="[^"]*uppercase[^"]*">pro<\/span>/);
  match(html, /76%/);
  match(html, /data-provider-api-price="openai-pro"/);
  match(html, />\$152\.00</);
  match(html, />≈ \$200\.00</);
  equal((html.match(/role="progressbar"/g) ?? []).length, 1);
  const stale = render(codexUsage, new Error("Temporary failure"), codexProvider);
  match(stale, /Showing stale totals/);
  match(stale, />\$152\.00</);
  doesNotMatch(stale, />≈ \$200\.00</);
  const expired = render({ ...codexUsage, api_price: { ...codexUsage.api_price!, period_end: now } }, undefined, codexProvider);
  match(expired, /Current cycle ended; refresh usage/);
  doesNotMatch(expired, />\$152\.00<|>≈ \$200\.00</);
});

test("Google AI Pro Antigravity and Gemini CLI OAuth cards use the canonical weekly price snapshot", () => {
  for (const currentProvider of googleProviders) {
    const html = render(googleUsage(currentProvider), undefined, currentProvider);
    match(html, /Google AI Pro Quota/);
    match(html, /class="[^"]*uppercase[^"]*">G1-PRO-TIER<\/span>/);
    match(html, new RegExp(`data-provider-api-price="${currentProvider.id}"`));
    match(html, /Total API price/);
    match(html, /Current-cycle API price/);
    match(html, /Estimated cycle API capacity/);
    equal(priceValues(html), "$30.00,$3.00,≈ $30.00");
    equal((html.match(/data-provider-api-price=/g) ?? []).length, 1);
    equal((html.match(/role="progressbar"/g) ?? []).length, 2);
    match(html, /5h used quota/);
    match(html, /Weekly used quota/);
    match(html, /Quota window: Weekly/);
    match(html, /Used for projection: 10%/);
    doesNotMatch(html, /≈ \$6\.00|Used for projection: 50%|<details|<summary/);
  }
});

test("Google subscription cards accept the backend's 5h-only cycle without guessing another window", () => {
  const period_end = now + 2 * 3600_000;
  for (const currentProvider of googleProviders) {
    const usage: ProviderUsage = { ...googleUsage(currentProvider),
      tiers: [{ name: "five_hour", used_percent: 25, resets_at: new Date(period_end).toISOString() }],
      api_price: { ...apiPrice, period_tier: "five_hour", period_start: period_end - 5 * 3600_000,
        period_end, used_percent: 25, estimated_capacity: [{ currency: "USD", total_cost: 12, priced_request_count: 1 }] },
    };
    const html = render(usage, undefined, currentProvider);
    equal(priceValues(html), "$30.00,$3.00,≈ $12.00");
    equal((html.match(/role="progressbar"/g) ?? []).length, 1);
    match(html, /Quota window: 5h/);
    match(html, /Used for projection: 25%/);
  }
});

test("Google weekly zero used keeps actual prices while capacity stays unknown even if 5h is used", () => {
  for (const currentProvider of googleProviders) {
    const original = googleUsage(currentProvider);
    for (const fiveHourUsed of [0, 50]) {
      const usage: ProviderUsage = { ...original,
        tiers: [{ ...original.tiers[0], used_percent: fiveHourUsed }, { ...original.tiers[1], used_percent: 0 }],
        api_price: { ...apiPrice, used_percent: 0, estimated_capacity: null },
      };
      const html = render(usage, undefined, currentProvider);
      equal(priceValues(html), "$30.00,$3.00,—");
      equal((html.match(/role="progressbar"/g) ?? []).length, 2);
      match(html, /aria-valuenow="0"/);
      match(html, /Quota window: Weekly/);
      doesNotMatch(html, /≈|Used for projection:/);
    }
  }
});

test("Google stale, refresh-error and expired snapshots keep recorded totals without a capacity estimate", () => {
  for (const currentProvider of googleProviders) {
    const usage = googleUsage(currentProvider);
    for (const html of [render(usage, new Error("Temporary Google refresh failure"), currentProvider),
      render({ ...usage, queried_at: new Date(now - 5 * 60_000).toISOString() }, undefined, currentProvider),
      render({ ...usage, queried_at: "invalid" }, undefined, currentProvider)]) {
      equal(priceValues(html), "$30.00,$3.00,—");
      match(html, /Showing stale totals; refresh usage to estimate capacity/);
      equal((html.match(/role="progressbar"/g) ?? []).length, 2);
    }
    const expired = render({ ...usage, api_price: { ...apiPrice, period_end: now } }, undefined, currentProvider);
    equal(priceValues(expired), "$30.00,—,—");
    match(expired, /Current cycle ended; refresh usage/);
    equal((expired.match(/role="progressbar"/g) ?? []).length, 2);
  }
});

test("Google catalog Gemini and unknown-duration windows render quota but do not create price cycles", () => {
  for (const currentProvider of googleProviders) {
    for (const name of ["gemini", "primary_window", "secondary_window"]) {
      const usage: ProviderUsage = { ...googleUsage(currentProvider),
        tiers: [{ name, used_percent: 25, resets_at: new Date(apiPrice.period_end!).toISOString() }],
        api_price: { ...apiPrice, period: null, period_start: null, period_end: null,
          period_tier: null, used_percent: null, estimated_capacity: null },
      };
      const html = render(usage, undefined, currentProvider);
      equal(priceValues(html), "$30.00,—,—");
      match(html, /Current cycle unknown; refresh usage/);
      equal((html.match(/role="progressbar"/g) ?? []).length, 1);
      doesNotMatch(html, /≈|\$0\.00/);
    }
  }
});

test("Google corrupt, mismatched or missing price snapshots keep the quota card renderable", () => {
  for (const currentProvider of googleProviders) {
    const original = googleUsage(currentProvider);
    for (const api_price of [undefined, null]) {
      const html = render({ ...original, api_price }, undefined, currentProvider);
      equal(priceValues(html), "—,—,—");
      match(html, /API price totals unavailable/);
      equal((html.match(/role="progressbar"/g) ?? []).length, 2);
    }
    for (const update of [{ period_start: NaN }, { period_end: Infinity }, { period_end: apiPrice.period_start },
      { period: null }]) {
      const html = render({ ...original, api_price: { ...apiPrice, ...update } }, undefined, currentProvider);
      equal(priceValues(html), "$30.00,—,—");
      match(html, /Current cycle unknown; refresh usage/);
      doesNotMatch(html, /NaN|Infinity|Invalid Date/);
    }
    for (const estimated_capacity of [null, [], [{ currency: "USD", total_cost: NaN, priced_request_count: 1 }]]) {
      const html = render({ ...original, api_price: { ...apiPrice, estimated_capacity } }, undefined, currentProvider);
      equal(priceValues(html), "$30.00,$3.00,—");
    }
    for (const tiers of [[{ ...original.tiers[1], used_percent: 11 }],
      [{ ...original.tiers[1], resets_at: new Date(apiPrice.period_end! + 1).toISOString() }],
      [original.tiers[1], original.tiers[1]]]) {
      equal(priceValues(render({ ...original, tiers }, undefined, currentProvider)), "$30.00,$3.00,—");
    }
    const unpriced = { ...apiPrice.period!, equivalent_costs: [], unpriced_request_count: 1 };
    const missingCyclePrice = render({ ...original, api_price: { ...apiPrice, period: unpriced } }, undefined, currentProvider);
    equal(priceValues(missingCyclePrice), "$30.00,—,—");
    match(missingCyclePrice, /1 attempts unpriced and excluded/);
  }
});

test("a downstream Nyro never renders Google administrator price summaries from forwarded usage", () => {
  const currentProvider: Provider = { ...provider, id: "nyro-downstream", vendor: "nyro-gateway", preset_key: "nyro-gateway",
    auth_mode: "apikey", base_url: "https://upstream.example.test" };
  const html = render({ ...priceUsage, provider_id: currentProvider.id, kind: "nyro_gateway", upstream: {
    redacted: ["api_price"], providers: googleProviders.map((google) => ({ id: google.id, name: google.name,
      vendor: google.vendor, routes: ["gemini-pro"], status: "ok", usage: googleUsage(google) })),
  } }, undefined, currentProvider);
  match(html, /Upstream Nyro Usage/);
  match(html, /Google AI Pro antigravity/);
  match(html, /Google AI Pro gemini-cli/);
  equal((html.match(/role="progressbar"/g) ?? []).length, 4);
  doesNotMatch(html, /data-provider-api-price|data-api-price=|Total API price|Current-cycle API price|Estimated cycle API capacity|\$30\.00|\$3\.00/);
});

test("provider price projection hides on refresh failures and stale observations while history remains", () => {
  for (const html of [render(priceUsage, new Error("Temporary failure")),
    render({ ...priceUsage, queried_at: new Date(now - 5 * 60_000).toISOString() }),
    render({ ...priceUsage, queried_at: "invalid" })]) {
    match(html, /data-provider-api-price/);
    match(html, /Showing stale totals; refresh usage to estimate capacity/);
    doesNotMatch(html, /≈ \$30\.00/);
    match(html, /\$30\.00/);
    match(html, /\$3\.00/);
  }
  match(render({ ...base, api_price: undefined }), /API price totals unavailable/);
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
