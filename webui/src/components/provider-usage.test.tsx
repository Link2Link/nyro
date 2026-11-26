import { doesNotMatch, equal, match, rejects } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { LocaleProvider } from "../lib/i18n";
import type { AccountPoolView, PoolQuotaWindow, ProviderAccountView } from "../lib/devin-account-pool";
import type { Provider, ProviderApiPriceStats, ProviderUsage, QuotaBucket } from "../lib/types";
import { GroupUsageSummary, ProviderUsageFooter } from "./provider-usage";
import { LocalUsageSummary } from "./local-usage-summary";

const now = Date.parse("2030-01-01T00:00:00Z");
const provider: Provider = {
  id: "usage", name: "Usage", vendor: "zai", protocol: "openai-compatible",
  base_url: "https://api.z.ai", use_proxy: false, fast_mode: "normal", is_enabled: true,
  created_at: "", updated_at: "",
};
const base: ProviderUsage = {
  provider_id: provider.id, kind: "glm_coding_plan", site: "global", tiers: [], balances: [], spends: [],
  is_available: true, queried_at: new Date(now).toISOString(),
};

function render(usage: ProviderUsage | undefined, error?: Error, currentProvider: Provider = provider, claudeAccount?: ProviderAccountView): string {
  const originalNow = Date.now;
  Date.now = () => now;
  const client = new QueryClient();
  const key = ["provider-usage", currentProvider.id];
  if (usage) client.setQueryData(key, usage);
  if (error) client.getQueryCache().find({ queryKey: key })!.setState({ status: "error", error });
  try {
    return renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      <ProviderUsageFooter provider={currentProvider} claudeAccount={claudeAccount} />
    </LocaleProvider></QueryClientProvider>);
  } finally {
    Date.now = originalNow;
    client.clear();
  }
}

const claudeProvider: Provider = { ...provider, id: "claude", vendor: "anthropic", channel: "claude-code", auth_mode: "oauth", base_url: "https://api.anthropic.com" };
const claudeAccount: ProviderAccountView = {
  account_id: "legacy", provider_id: "claude", label: "Legacy", is_enabled: true, priority: 0,
  status: "connected", status_version: 1, verified_plan: null, identity_verified: true,
  scheduling: "included", daily: null, weekly: null, reserve_usd: null,
  observed_at: now - 8 * 60_000, stale: true, updated_at: "",
  claude_usage: { windows: [
    { name: "five_hour", used_percent: 38, resets_at: "2030-01-01T05:00:00Z" },
    { name: "weekly_limit", used_percent: 21, resets_at: "2030-01-05T00:00:00Z" },
  ] },
};
const claudePool: AccountPoolView = {
  kind: "claude_code", provider_id: "claude", config: null, accounts: [claudeAccount],
  quota: { daily: null, weekly: null, estimated: false }, readiness: "eligible",
  counts: { total: 1, accounting: 1, available: 1, included: 1, reserve: 0, pro: 0, max: 0, unclassified: 1,
    unknown: 0, disabled: 0, disconnected: 0, auth_required: 0, quota_exhausted: 0, plan_mismatch: 0, plan_unverified: 0 },
};
const claudeUsage: ProviderUsage = { ...base, provider_id: "claude", kind: "claude_code_plan", account_pool: claudePool,
  tiers: claudeAccount.claude_usage!.windows, queried_at: new Date(claudeAccount.observed_at!).toISOString() };
const withAccount = (account: Partial<ProviderAccountView>): ProviderUsage => ({ ...claudeUsage,
  account_pool: { ...claudePool, accounts: [{ ...claudeAccount, ...account }] } });
const headerWindow = (name: string, used_percent: number, observed_at: number, resets_at: string | null = null) =>
  ({ name, used_percent, resets_at, source: "inference_header" as const, observed_at });
/** Header freshness label only; rows carry their own reset <time> elements. */
const freshness = (html: string) => html.match(/<span[^>]*data-claude-freshness="[^"]*"[^>]*>[\s\S]*?<\/span>/)?.[0] ?? "";
const pad = (value: number) => String(value).padStart(2, "0");
function localClock(epochMs: number): string {
  const date = new Date(epochMs);
  const time = `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
  return date.toDateString() === new Date(now).toDateString() ? time : `${date.getMonth() + 1}/${date.getDate()} ${time}`;
}
const rowOf = (html: string, name: string) => html.split(`data-claude-window="${name}"`)[1]?.split("data-claude-window=")[0] ?? "";

test("Claude singleton time is the older main-window observation and staleness follows the full query", () => {
  const html = render(claudeUsage, undefined, claudeProvider);
  const header = freshness(html);
  match(header, /data-claude-freshness="stale"/); match(header, />Stale · <time dateTime="2029-12-31T23:52:00.000Z">8m ago<\/time>/);
  doesNotMatch(html, /Updated|Observed|just now/);
  const fresh = freshness(render(withAccount({ stale: false }), undefined, claudeProvider));
  match(fresh, /data-claude-freshness="fresh"/); match(fresh, />8m ago</);
  doesNotMatch(fresh, /Stale|Updated|Observed/);
  const failedFull = freshness(render(withAccount({ observed_at: now, stale: true }), undefined, claudeProvider));
  match(failedFull, /Stale · <time[^>]*>just now</);
  // Different main-window times: the older one is shown, as the backend's queried_at.
  const mixed = freshness(render(withAccount({ stale: true, claude_usage: { windows: [
    headerWindow("five_hour", 38, now - 60_000), headerWindow("weekly_limit", 21, now - 3 * 60_000),
  ] } }), undefined, claudeProvider));
  match(mixed, /data-claude-freshness="fresh"/); match(mixed, />3m ago</);
});

test("Claude full-query windows age beyond 660 seconds even if cached stale is false", () => {
  for (const age of [660_000, 660_001]) {
    const header = freshness(render(withAccount({ observed_at: now - age, stale: false }), undefined, claudeProvider));
    equal(header.includes("Stale"), age > 660_000);
    match(header, /11m ago/);
  }
});

test("Claude missing main windows or invalid times show Stale without inventing a time", () => {
  for (const observed_at of [null, 0, -1, NaN, Infinity, 8_640_000_000_000_001, now + 60_001]) {
    const html = render(withAccount({ observed_at, stale: false }), undefined, claudeProvider);
    const header = freshness(html);
    match(header, />Stale</);
    doesNotMatch(header, /<time|ago|just now/);
    doesNotMatch(html, /Updated|Observed|NaN|Infinity|Invalid Date/);
  }
  for (const windows of [[], [claudeAccount.claude_usage!.windows[0]], [claudeAccount.claude_usage!.windows[1]]]) {
    const header = freshness(render(withAccount({ observed_at: now, stale: false, claude_usage: { windows } }), undefined, claudeProvider));
    match(header, />Stale</); doesNotMatch(header, /<time/);
  }
  // Without a usable account the response tiers and backend observation time are used.
  for (const account_pool of [undefined, null, { ...claudePool, accounts: [] },
    { ...claudePool, accounts: [claudeAccount, { ...claudeAccount, account_id: "other" }] }]) {
    const html = render({ ...claudeUsage, account_pool }, undefined, claudeProvider);
    match(freshness(html), /data-claude-freshness="fresh"[^>]*>[\s\S]*>8m ago</);
    equal((html.match(/role="progressbar"/g) ?? []).length, 2);
    doesNotMatch(html, /data-claude-source|Full query|From chat responses/);
    const unknown = freshness(render({ ...claudeUsage, account_pool, queried_at: "" }, undefined, claudeProvider));
    match(unknown, />Stale</); doesNotMatch(unknown, /<time/);
  }
});

test("Claude response-header main windows stay fresh when the full query is stale and name their source on hover", () => {
  const html = render(withAccount({ stale: true, observed_at: now - 30 * 60_000, claude_usage: { windows: [
    headerWindow("five_hour", 82.5, now - 2 * 60_000, "2030-01-01T05:00:00Z"),
    headerWindow("weekly_limit", 21, now - 60_000, "2030-01-05T00:00:00Z"),
    { name: "feature:sonnet:weekly_limit", used_percent: 4, resets_at: null },
  ] } }), undefined, claudeProvider);
  match(freshness(html), /data-claude-freshness="fresh"/); match(freshness(html), />2m ago</);
  const five = rowOf(html, "five_hour");
  match(five, new RegExp(`title="From chat responses · ${localClock(now - 2 * 60_000)}"`));
  match(five, /aria-valuenow="82.5"/); match(five, />82.5%</);
  match(rowOf(html, "feature:sonnet:weekly_limit"), new RegExp(`title="Full query · ${localClock(now - 30 * 60_000)}"`));
  equal((html.match(/data-claude-window=/g) ?? []).length, 3);
  equal((html.match(/role="progressbar"/g) ?? []).length, 3);
  doesNotMatch(html, /data-claude-quota-observation|\(partial\)|Status:|Source:|inference_header<|Older than|Newer than|aria-valuenow="38"/);
});

test("Claude footer marks only stale model windows; stale or expired main windows switch the header to Stale", () => {
  const fresh = render(withAccount({ stale: false, observed_at: now - 60_000, claude_usage: { windows: [
    ...claudeAccount.claude_usage!.windows,
    headerWindow("feature:fable:weekly_limit", 6, now - 20 * 60_000),
  ] } }), undefined, claudeProvider);
  match(freshness(fresh), /data-claude-freshness="fresh"/);
  equal((fresh.match(/data-claude-stale-mark/g) ?? []).length, 1);
  match(rowOf(fresh, "feature:fable:weekly_limit"), /data-claude-stale-mark[^>]*>Stale</);
  const staleWeekly = render(withAccount({ stale: true, claude_usage: { windows: [
    headerWindow("five_hour", 38, now - 60_000), headerWindow("weekly_limit", 21, now - 20 * 60_000),
  ] } }), undefined, claudeProvider);
  match(freshness(staleWeekly), />Stale · <time[^>]*>20m ago</);
  doesNotMatch(staleWeekly, /data-claude-stale-mark/);
  const expired = render(withAccount({ stale: false, observed_at: now - 60_000, claude_usage: { windows: [
    { name: "five_hour", used_percent: 64, resets_at: new Date(now - 1).toISOString() },
    claudeAccount.claude_usage!.windows[1],
  ] } }), undefined, claudeProvider);
  match(freshness(expired), />Stale · /);
  match(rowOf(expired, "five_hour"), />64%</);
  doesNotMatch(rowOf(expired, "five_hour"), /progressbar|>0%</);
  equal((expired.match(/role="progressbar"/g) ?? []).length, 1);
});

test("a failed Claude usage query stays visible, marks full-query windows stale and keeps fresher header windows", () => {
  const failed = render(withAccount({ observed_at: now, stale: false }), new Error("Claude upstream refresh failed"), claudeProvider);
  match(failed, /role="alert"[\s\S]*Usage query failed: Claude upstream refresh failed/);
  match(freshness(failed), />Stale · /);
  const headers = render(withAccount({ stale: false, claude_usage: { windows: [
    headerWindow("five_hour", 38, now - 60_000), headerWindow("weekly_limit", 21, now - 60_000),
  ] } }), new Error("Claude upstream refresh failed"), claudeProvider);
  match(headers, /Usage query failed: Claude upstream refresh failed/);
  match(freshness(headers), /data-claude-freshness="fresh"[^>]*>[\s\S]*>1m ago</);
});

test("Claude footer shows the more recent of the local pool read and the nested usage read", () => {
  const nested = withAccount({ observed_at: now - 60_000, stale: false });
  const reads: [ProviderAccountView, string][] = [
    [{ ...claudeAccount, observed_at: now - 3 * 60_000, stale: false }, "1m ago"],
    [{ ...claudeAccount, observed_at: now, stale: false }, "just now"],
    [{ ...claudeAccount, observed_at: now - 3 * 60_000, stale: true, claude_usage: { windows: [
      headerWindow("five_hour", 5, now), headerWindow("weekly_limit", 6, now),
    ] } }, "just now"],
  ];
  for (const [local, expected] of reads) {
    const html = render(nested, undefined, claudeProvider, local);
    match(freshness(html), new RegExp(`data-claude-freshness="fresh"[^>]*>[\\s\\S]*>${expected}<`));
  }
  // Before the usage query has data, the local account still renders compact rows.
  const localOnly = render(undefined, undefined, claudeProvider, { ...claudeAccount, observed_at: now, stale: false });
  equal((localOnly.match(/data-claude-window=/g) ?? []).length, 2);
  match(freshness(localOnly), />just now</);
});

test("Claude footer and folded group do not retry a failed usage attempt into cooldown-cache success", async () => {
  for (const folded of [false, true]) {
    const client = new QueryClient();
    try {
      renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
        {folded ? <GroupUsageSummary providers={[claudeProvider]} /> : <ProviderUsageFooter provider={claudeProvider} />}
      </LocaleProvider></QueryClientProvider>);
      const query = client.getQueryCache().find({ queryKey: ["provider-usage", claudeProvider.id] })!;
      equal(query.options.retry, false);
      let attempts = 0;
      await rejects(query.fetch({ ...query.options, retryDelay: 0, queryFn: async () => {
        if (++attempts === 1) throw new Error("First upstream failure");
        return claudeUsage; // Backend cooldown would serve unchanged cached usage.
      } }), /First upstream failure/);
      equal(attempts, 1);
      equal(query.state.status, "error");
      equal(query.state.data, undefined);
    } finally { client.clear(); }
  }
});

test("non-Claude usage keeps its existing retry policy", () => {
  const client = new QueryClient();
  try {
    renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      <ProviderUsageFooter provider={provider} />
    </LocaleProvider></QueryClientProvider>);
    equal(client.getQueryCache().find({ queryKey: ["provider-usage", provider.id] })!.options.retry, 1);
  } finally { client.clear(); }
});

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
const sub2apiProvider: Provider = { ...provider, id: "sub2api", name: "Example relay", vendor: "openai", preset_key: "openai",
  channel: "sub2api", protocol: "openai-responses", auth_mode: "apikey", base_url: "https://relay.example.test" };
const sub2apiPrice: ProviderApiPriceStats = { ...apiPrice, period_start: now, period_end: Date.parse("2030-02-01T00:00:00Z"),
  period_tier: "calendar_month", used_percent: null, estimated_capacity: null };
const sub2apiUsage: ProviderUsage = { ...base, provider_id: sub2apiProvider.id, kind: "sub2api", site: "local", is_available: null,
  api_price: sub2apiPrice };

test("Sub2API cards show retained attempts, tokens, and two amounts without upstream quota or projections", () => {
  const html = render(sub2apiUsage, undefined, sub2apiProvider);
  match(html, />Sub2API Usage</);
  match(html, /data-local-usage="sub2api"/);
  match(html, /data-local-usage-scope="all_time"[^]*>All time<[^]*data-local-usage-scope="calendar_month"[^]*>This month</);
  match(html, /UTC calendar month/);
  match(html, /not Sub2API billing/);
  equal(priceValues(html), "2,1.00M,$30.00,1,100.0K,$3.00");
  doesNotMatch(html, /role="progressbar"|Quota reset|Estimated cycle API capacity|Quota-adjusted prices|Current-cycle API price|No usage data/);

  const wrongSnapshot = render({ ...priceUsage, provider_id: sub2apiProvider.id }, undefined, sub2apiProvider);
  equal(priceValues(wrongSnapshot), "—,—,—,—,—,—");
  doesNotMatch(wrongSnapshot, /role="progressbar"|≈/);
  const stale = render(sub2apiUsage, new Error("Price lookup failed"), sub2apiProvider);
  match(stale, /Usage query failed/);
  equal(priceValues(stale), priceValues(html));
});

test("Sub2API distinguishes expired months, unused months and unpriced attempts", () => {
  const expired = renderToStaticMarkup(<LocalUsageSummary kind="sub2api" stats={sub2apiPrice} now={sub2apiPrice.period_end!} loading={false} locale="zh-CN" />);
  match(expired, />累计<[^]*>本月</);
  match(expired, /UTC 自然月|本月统计尚未更新/);
  equal(priceValues(expired), "2,1.00M,$30.00,—,—,—");

  const zero = { total_tokens: 0, request_count: 0, equivalent_costs: [], unpriced_request_count: 0 };
  const empty = renderToStaticMarkup(<LocalUsageSummary kind="sub2api" stats={{ ...sub2apiPrice, all_time: zero, period: zero }} now={now} loading={false} />);
  equal(priceValues(empty), "0,0,$0.00,0,0,$0.00");
  const unpriced = { ...sub2apiPrice.period!, equivalent_costs: [], unpriced_request_count: 1 };
  const unknownPrice = renderToStaticMarkup(<LocalUsageSummary kind="sub2api" stats={{ ...sub2apiPrice, period: unpriced }} now={now} loading={false} />);
  equal(priceValues(unknownPrice), "2,1.00M,$30.00,1,100.0K,—");
  match(unknownPrice, /1 attempts unpriced and excluded/);
});

test("folded quota summaries do not query Sub2API local accounting", () => {
  const client = new QueryClient();
  try {
    const html = renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      <GroupUsageSummary providers={[sub2apiProvider]} />
    </LocaleProvider></QueryClientProvider>);
    equal(html, "");
    equal(client.getQueryCache().find({ queryKey: ["provider-usage", sub2apiProvider.id] }), undefined);
  } finally { client.clear(); }
});

const anthropicProvider: Provider = { ...provider, id: "anthropic", name: "Anthropic", vendor: "anthropic", preset_key: "anthropic",
  channel: "default", protocol: "anthropic-messages", auth_mode: "apikey", base_url: "https://api.anthropic.com" };
const anthropicUsage: ProviderUsage = { ...sub2apiUsage, provider_id: anthropicProvider.id, kind: "anthropic_api" };

test("Anthropic default-channel cards show the same local totals without upstream quota", () => {
  const html = render(anthropicUsage, undefined, anthropicProvider);
  match(html, />Anthropic API Usage</);
  match(html, /data-local-usage="anthropic_api"/);
  match(html, /data-local-usage-scope="all_time"[^]*>All time<[^]*data-local-usage-scope="calendar_month"[^]*>This month</);
  match(html, /not upstream billing/);
  doesNotMatch(html, /Sub2API/);
  equal(priceValues(html), "2,1.00M,$30.00,1,100.0K,$3.00");
  doesNotMatch(html, /role="progressbar"|Quota reset|Estimated cycle API capacity|No usage data/);
  // A cached report from another accounting mode is never shown as this one.
  equal(priceValues(render({ ...sub2apiUsage, provider_id: anthropicProvider.id }, undefined, anthropicProvider)), "—,—,—,—,—,—");
  equal(priceValues(render({ ...priceUsage, provider_id: anthropicProvider.id }, undefined, anthropicProvider)), "—,—,—,—,—,—");

  const zh = renderToStaticMarkup(<LocalUsageSummary kind="anthropic_api" stats={anthropicUsage.api_price} now={now} loading={false} locale="zh-CN" />);
  match(zh, />累计<[^]*>本月</);
  match(zh, /非上游实际账单/);

  const client = new QueryClient();
  try {
    equal(renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      <GroupUsageSummary providers={[anthropicProvider]} />
    </LocaleProvider></QueryClientProvider>), "");
    equal(client.getQueryCache().find({ queryKey: ["provider-usage", anthropicProvider.id] }), undefined);
  } finally { client.clear(); }
});

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

test("Google family snapshots replace parent quota while footer and group share conservative family availability", () => {
  for (const currentProvider of googleProviders) {
    const antigravity = currentProvider.channel === "antigravity";
    const original = googleUsage(currentProvider);
    const raw: QuotaBucket = { id: "raw-flash", scope_id: "scope", label: "Original Flash", models: ["gemini-3.8-flash", "gemini-3.8-flash-high"],
      binding: { status: "window_constraint", source: "configured", revision: 1 }, tiers: original.tiers, constraint_ids: [],
      source: "enforced_summary", queried_at: original.queried_at, stale: false,
      scheduling: { status: "eligible", blocking_tiers: [] }, api_price: apiPrice };
    const flash: QuotaBucket = { ...raw, id: "display-family-flash", label: antigravity ? "Gemini Models" : "gemini-3.8-flash", constraint_ids: [raw.id],
      binding: { ...raw.binding, status: "resolved", source: "google_family_display_v1" }, source: "google_family_display_v1" };
    const argon: QuotaBucket = { ...flash, id: "display-family-argon", label: antigravity ? "Gemini 4 Argon" : "gemini-4-argon",
      models: ["gemini-4-argon"], tiers: antigravity ? original.tiers.filter(tier => tier.name === "weekly_limit") : original.tiers, constraint_ids: [] };
    const thirdParty: QuotaBucket = { ...flash, id: "display-3p", label: "Claude and GPT", models: ["claude-sonnet-4-6", "gpt-oss-120b-medium"], constraint_ids: [] };
    const cards = antigravity ? [flash, thirdParty, argon] : [flash, argon];
    const full = [...cards, raw];
    const html = render({ ...original, quota_buckets: full }, undefined, currentProvider);
    equal((html.match(/data-quota-bucket=/g) ?? []).length, cards.length);
    equal((html.match(/role="progressbar"/g) ?? []).length, antigravity ? 5 : 4);
    equal((html.match(/data-api-price="all_time"/g) ?? []).length, cards.length + 1);
    equal((html.match(/data-api-price="period"/g) ?? []).length, cards.length);
    match(html, new RegExp(`data-quota-availability="available"[^]*Available · ${cards.length}/${cards.length}`));
    for (const card of cards) match(html, new RegExp(`>${card.label}</h3>`));
    match(html, /Model bindings|Source bucket details/);
    doesNotMatch(html, /data-quota-detail="[^">]*display-family/);
    const single = render({ ...original, quota_buckets: [flash, raw] }, undefined, currentProvider);
    match(single, new RegExp(`data-quota-availability="partial"[^]*Partially available · 1/${cards.length}`));
    const client = new QueryClient();
    const originalNow = Date.now;
    Date.now = () => now;
    try {
      for (const quota_buckets of [full, [flash, raw], [raw], [], [flash, { ...argon, scope_id: "other-owner" }, raw]]) {
        client.setQueryData(["provider-usage", currentProvider.id], { ...original, quota_buckets });
        const group = renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider><GroupUsageSummary providers={[currentProvider]} /></LocaleProvider></QueryClientProvider>);
        match(group, quota_buckets === full ? new RegExp(`Available · ${cards.length}/${cards.length}`)
          : quota_buckets[0] === flash && quota_buckets.length === 2 ? /data-quota-availability="partial"/ : /data-quota-availability="unknown"/);
        doesNotMatch(group, /Peak used|progressbar|%/);
      }
    } finally { Date.now = originalNow; client.clear(); }
    for (const quota_buckets of [[], [raw]]) {
      const empty = render({ ...original, quota_buckets }, undefined, currentProvider);
      match(empty, /data-quota-availability="unknown"/);
      equal((empty.match(/No quota observations for the target model families/g) ?? []).length, 1);
      equal((empty.match(/data-api-price="all_time"/g) ?? []).length, 1);
      match(empty, /Model bindings/);
      doesNotMatch(empty, /data-quota-bucket=|role="progressbar"|data-api-price="period"|data-api-price="estimated_capacity"|\$0\.00/);
    }
    // Old servers omit the field; only those snapshots preserve the legacy parent presentation.
    equal(render({ ...original, quota_buckets: undefined }, undefined, currentProvider), render(original, undefined, currentProvider));
  }
});

const catalogFamily = (id: string, label: string, used: number): QuotaBucket => ({
  id, scope_id: "google-owner", label, models: [label], source: "google_family_display_v1",
  binding: { status: "unresolved", source: "google_family_display_v1", revision: 1 },
  queried_at: new Date(now).toISOString(), stale: false, constraint_ids: [],
  tiers: [{ name: "catalog", used_percent: used }],
  scheduling: { status: "eligible", reason: "unknown_mapping", blocking_tiers: [] },
});

test("Google footer distinguishes fresh observed usage from confirmed availability and visible refresh failures", () => {
  const currentProvider = googleProviders[0];
  const usage: ProviderUsage = { ...base, provider_id: currentProvider.id, kind: "google_subscription",
    quota_buckets: [catalogFamily("family-flash", "gemini-3.8-flash", 0), catalogFamily("family-argon", "gemini-4-argon", 3.8)],
  };
  const fresh = render(usage, undefined, currentProvider);
  match(fresh, /data-quota-availability="observed"[^]*Usage updated · 2\/3/);
  equal((fresh.match(/data-bucket-status="observed"/g) ?? []).length, 2);
  equal((fresh.match(/Quota binding pending confirmation/g) ?? []).length, 2);
  match(fresh, /Model catalog usage/);
  match(fresh, /aria-valuenow="0"/);
  doesNotMatch(fresh, /Usage query failed|data-quota-availability="available"|data-bucket-status="available"|Unavailable/);
  const failed = render(usage, new Error("Temporary Google refresh failure"), currentProvider);
  match(failed, /role="alert"[^]*Usage query failed: Temporary Google refresh failure/);
  match(failed, /Refresh failed/);
  match(failed, /Stale/);
  equal((failed.match(/role="progressbar"/g) ?? []).length, 2);
  doesNotMatch(failed, /data-quota-availability="observed"|data-bucket-status="observed"|Usage updated/);
  const aged = render({ ...usage, quota_buckets: usage.quota_buckets!.map((bucket) => ({ ...bucket,
    queried_at: new Date(now - 5 * 60_000).toISOString() })) }, undefined, currentProvider);
  match(aged, /data-quota-availability="unknown"[^]*Stale/);
  doesNotMatch(aged, /Usage updated|Usage query failed/);
  const unavailable = render({ ...usage, is_available: false }, undefined, currentProvider);
  match(unavailable, /data-quota-availability="unavailable"[^]*Account unavailable/);
  doesNotMatch(unavailable, /data-quota-availability="observed"|Usage updated/);
});

test("Google card and folded group use the real-provider usage cache, never the synthetic published ID", () => {
  const currentProvider = googleProviders[0];
  const real: ProviderUsage = { ...base, provider_id: currentProvider.id, kind: "google_subscription",
    quota_buckets: [catalogFamily("family-flash", "gemini-3.8-flash", 0), catalogFamily("family-argon", "gemini-4-argon", 3.8)],
  };
  const client = new QueryClient();
  const originalNow = Date.now;
  Date.now = () => now;
  client.setQueryData(["provider-usage", currentProvider.id], real);
  client.setQueryData(["provider-usage", "qb_published_family_id"], { ...real, quota_buckets: [],
    scheduling: { status: "quota_exhausted", reason: "account_unavailable", blocking_tiers: [] } });
  try {
    const html = renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      <ProviderUsageFooter provider={currentProvider} /><GroupUsageSummary providers={[currentProvider]} />
    </LocaleProvider></QueryClientProvider>);
    equal((html.match(/data-quota-availability="observed"/g) ?? []).length, 2);
    equal((html.match(/Usage updated · 2\/3/g) ?? []).length, 2);
    doesNotMatch(html, /Account unavailable|No quota observations/);
    equal(client.getQueryCache().findAll({ queryKey: ["provider-usage"] }).length, 2);
  } finally { Date.now = originalNow; client.clear(); }
});

test("non-Google bucket cards and empty snapshots retain the legacy presentation", () => {
  const raw: QuotaBucket = { id: "raw-model", scope_id: "scope", label: "Other provider model", models: ["model-high"],
    binding: { status: "resolved", source: "configured", revision: 1 }, tiers: priceUsage.tiers, constraint_ids: [],
    source: "enforced_summary", queried_at: priceUsage.queried_at, stale: false,
    scheduling: { status: "eligible", blocking_tiers: [] }, api_price: apiPrice };
  const html = render({ ...priceUsage, quota_buckets: [raw] });
  equal((html.match(/data-quota-bucket=/g) ?? []).length, 1);
  match(html, /Other provider model|model-high|Bucket details/);
  match(html, /data-quota-availability="available"[^]*1\/1/);
  doesNotMatch(html, /data-bucket-diagnostics|data-provider-quota-diagnostics/);
  equal(render({ ...priceUsage, quota_buckets: [] }), render(priceUsage));
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

const nyroProvider: Provider = { ...provider, id: "nyro-chain", name: "Edge Nyro", vendor: "nyro-gateway",
  preset_key: "nyro-gateway", base_url: "http://remote.test:19530" };
const forwardedRouteBuckets = (): QuotaBucket[] => Array.from({ length: 18 }, (_, index) => ({
  id: `forwarded-route-${index}`, scope_id: "local-forwarded-scope", label: `route-only-card-${index}`,
  models: [`remote-route-${index}`], binding: { status: "resolved", source: "forwarded_route_v1", revision: 1 },
  source: "forwarded_route", tiers: [], constraint_ids: [], queried_at: new Date(now).toISOString(), stale: false,
  scheduling: { status: "quota_exhausted", reason: "remote_route_exhausted", blocking_tiers: ["five_hour"] },
}));
const forwardedPoolWindow = (over: Partial<PoolQuotaWindow> = {}): PoolQuotaWindow => ({
  total_units: 2, total_accounts: 2, capacity_complete: true, known_accounts: 2,
  known_remaining_units: 0.8, used_units: 1.2, remaining_units: 0.8, used_percent: 60,
  remaining_percent: 40, stale: false, next_reset_at: new Date(now + 3600_000).toISOString(),
  next_reset_accounts: 1, reset_mode: "staggered", ...over,
});
function nyroUsage(remote: ProviderUsage): ProviderUsage {
  return { ...base, provider_id: nyroProvider.id, kind: "nyro_gateway", quota_buckets: forwardedRouteBuckets(), upstream: {
    object: "nyro.usage", version: 2, generated_at: new Date(now).toISOString(), providers: [
      { id: "remote-quota", name: "Remote actual quota", vendor: "anthropic", routes: ["claude-main"], status: "ok", usage: remote },
    ],
  } };
}
function renderNyroGroup(usage: ProviderUsage): string {
  const originalNow = Date.now;
  Date.now = () => now;
  const client = new QueryClient();
  client.setQueryData(["provider-usage", nyroProvider.id], usage);
  try {
    return renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      <GroupUsageSummary providers={[nyroProvider]} />
    </LocaleProvider></QueryClientProvider>);
  } finally { Date.now = originalNow; client.clear(); }
}

test("forwarded route-admission buckets never hide a chained Nyro's actual upstream usage", () => {
  const value = nyroUsage(upstreamUsage({ kind: "claude_code_plan", tiers: [{ name: "five_hour", used_percent: 41 }] }));
  const html = render(value, undefined, nyroProvider);
  match(html, /Upstream Nyro Usage/);
  match(html, /Remote actual quota/);
  match(html, /Routes: claude-main/);
  match(html, /aria-valuenow="41"/);
  equal((html.match(/role="progressbar"/g) ?? []).length, 1);
  doesNotMatch(html, /data-quota-bucket=|route-only-card|forwarded_route_v1|Model bindings|Retained attempts|Recorded tokens|API price|Bucket details/);
  equal(render({ ...value, quota_buckets: undefined }, undefined, nyroProvider), html);
});

test("forwarded anonymous pool quota replaces duplicate flat windows and keeps unrelated tiers", () => {
  const value = nyroUsage(upstreamUsage({ kind: "devin_plan", tiers: [
    { name: "daily", used_percent: 99 }, { name: "monthly", used_percent: 18 },
  ], pool_quota: { daily: forwardedPoolWindow(), weekly: null, estimated: false } }));
  const html = render(value, undefined, nyroProvider);
  match(html, /Account pool quota/);
  match(html, /aria-valuenow="60"/);
  match(html, /aria-valuenow="18"/);
  equal((html.match(/role="progressbar"/g) ?? []).length, 2);
  match(html, /Next account reset/);
  doesNotMatch(html, /aria-valuenow="99"|Quota reset|full.pool|data-quota-bucket=|API price/);
});

test("forwarded anonymous Claude pools render five-hour and weekly quotas with honest reset labels", () => {
  const value = nyroUsage(upstreamUsage({ kind: "claude_code_plan", tiers: [], pool_quota: {
    daily: null, weekly: null, estimated: true, claude_windows: [
      { name: "weekly_limit", quota: forwardedPoolWindow({ used_percent: 75, reset_mode: "aligned", next_reset_accounts: 2 }) },
      { name: "five_hour", quota: forwardedPoolWindow({ used_percent: 40 }) },
    ],
  } }));
  const html = render(value, undefined, nyroProvider);
  match(html, /Account pool quota/);
  match(html, /aria-valuenow="40"/);
  match(html, /aria-valuenow="75"/);
  match(html, /Next account reset/);
  match(html, /Aligned reset/);
  equal((html.match(/role="progressbar"/g) ?? []).length, 2);
  doesNotMatch(html, /Quota reset|full.pool|API price/);
});

test("incomplete unknown and stale forwarded pool observations never invent utilization or recovery", () => {
  for (const over of [{ capacity_complete: false, known_accounts: 1, used_percent: null },
    { used_percent: null }, { stale: true }, { total_units: 0 }]) {
    const window = forwardedPoolWindow(over);
    const value = nyroUsage(upstreamUsage({ kind: "devin_plan", tiers: [], pool_quota: { daily: window, weekly: null, estimated: false } }));
    const html = render(value, undefined, nyroProvider);
    match(html, /Account pool quota/);
    match(html, /Capacity incomplete|Usage unknown/);
    match(html, /Usage coverage/);
    match(html.replace(/<[^>]*>/g, "").replace(/\s/g, ""), new RegExp(window.known_accounts + "/" + window.total_accounts + "accounts"));
    if (window.stale) match(html, /Stale/);
    doesNotMatch(html, /role="progressbar"|aria-valuenow="0"|Quota reset|full.pool|NaN|Infinity/);
  }
});

test("folded Nyro group summary uses actual upstream tiers and anonymous pool quota, not route availability", () => {
  const remote = upstreamUsage({ kind: "devin_plan", tiers: [{ name: "five_hour", used_percent: 41 }],
    pool_quota: { daily: forwardedPoolWindow({ used_percent: 80 }), weekly: null, estimated: false } });
  const value = nyroUsage(remote);
  const group = renderNyroGroup(value);
  match(group, /Peak used/);
  match(group, /80%/);
  doesNotMatch(group, /data-quota-availability|route-only-card|paused/);
  equal(renderNyroGroup({ ...value, quota_buckets: undefined }), group);
  const unknown = renderNyroGroup(nyroUsage({ ...remote, tiers: [], pool_quota: {
    daily: forwardedPoolWindow({ capacity_complete: false, used_percent: 100 }), weekly: null, estimated: false,
  } }));
  doesNotMatch(unknown, /Peak used|100%|0%|data-quota-availability/);
});

test("folded Nyro summary replaces same-name flat quota with complete or unknown anonymous pool usage", () => {
  const remote = upstreamUsage({ kind: "devin_plan", tiers: [{ name: "daily", used_percent: 99 }],
    pool_quota: { daily: forwardedPoolWindow({ used_percent: 60 }), weekly: null, estimated: false } });
  const complete = renderNyroGroup(nyroUsage(remote));
  match(complete, /Peak used/);
  match(complete, /60%/);
  doesNotMatch(complete, /99%|data-quota-availability/);
  const unknown = renderNyroGroup(nyroUsage({ ...remote, tiers: [{ name: "daily", used_percent: 100 }], pool_quota: {
    daily: forwardedPoolWindow({ used_percent: null }), weekly: null, estimated: false,
  } }));
  doesNotMatch(unknown, /Peak used|100%|0%|data-quota-availability/);
});

test("conflicting forwarded pool windows never revive same-name flat quota or a healthy summary", () => {
  const value = nyroUsage(upstreamUsage({ kind: "devin_plan", tiers: [
    { name: "daily", used_percent: 99 }, { name: "weekly_limit", used_percent: 10 },
  ], pool_quota: { daily: forwardedPoolWindow({ used_percent: null }),
    weekly: forwardedPoolWindow({ used_percent: 20 }), estimated: false,
    claude_windows: [{ name: "weekly_limit", quota: forwardedPoolWindow({ used_percent: 90 }) }],
  } }));
  const html = render(value, undefined, nyroProvider);
  match(html, /Account pool quota/);
  match(html, /Conflicting account pool observations; usage unknown/);
  match(html, /Usage unknown/);
  doesNotMatch(html, /role="progressbar"|aria-valuenow|99%|10%|20%|90%|data-quota-availability/);
  doesNotMatch(renderNyroGroup(value), /Peak used|99%|10%|20%|90%|data-quota-availability/);
});

test("an empty forwarded upstream scope preserves no-routes feedback despite local route-admission buckets", () => {
  const value = { ...nyroUsage(upstreamUsage({ tiers: [] })), upstream: { providers: [] } };
  const html = render(value, undefined, nyroProvider);
  match(html, /Upstream Nyro Usage/);
  match(html, /This key has no routes on the remote/);
  doesNotMatch(html, /data-quota-bucket=|route-only-card|Model bindings|API price|Bucket details|role="progressbar"/);
  equal(render({ ...value, quota_buckets: undefined }, undefined, nyroProvider), html);
});
