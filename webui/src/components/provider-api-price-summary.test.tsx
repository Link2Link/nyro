import { doesNotMatch, equal, match, ok } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { providerApiPriceSupported } from "../lib/provider-usage";
import type { AccountUsageTotals, ProviderApiPriceStats, ProviderUsage } from "../lib/types";
import { ProviderApiPriceSummary } from "./provider-api-price-summary";

const end = Date.parse("2030-01-07T13:48:02Z");
const start = end - 7 * 86_400_000;
const now = start + 86_400_000;
const zero: AccountUsageTotals = { total_tokens: 0, request_count: 0, equivalent_costs: [], unpriced_request_count: 0 };
const period: AccountUsageTotals = { total_tokens: 1_234_567, request_count: 3,
  equivalent_costs: [{ currency: "USD", total_cost: 21.4, priced_request_count: 3 }], unpriced_request_count: 0 };
const stats: ProviderApiPriceStats = {
  all_time: { ...period, equivalent_costs: [{ currency: "USD", total_cost: 228.79, priced_request_count: 3 }] },
  period, period_start: start, period_end: end, period_tier: "weekly_limit", used_percent: 10,
  estimated_capacity: [{ currency: "USD", total_cost: 214, priced_request_count: 3 }],
};
const usage: ProviderUsage = {
  provider_id: "provider-a", kind: "glm_coding_plan", site: "global", balances: [], spends: [],
  queried_at: new Date(now).toISOString(), api_price: stats,
  tiers: [{ name: "five_hour", used_percent: 50, resets_at: new Date(now + 3600_000).toISOString() },
    { name: "weekly_limit", used_percent: 10, resets_at: new Date(end).toISOString() }],
};
const props = { now, stale: false, loading: false, locale: "en-US" as const };
const render = (value = usage, overrides: Partial<Parameters<typeof ProviderApiPriceSummary>[0]> = {}) =>
  renderToStaticMarkup(<ProviderApiPriceSummary {...props} usage={value} {...overrides} />);
const values = (html: string) => [...html.matchAll(/<dd[^>]*>(.*?)<\/dd>/g)].map((result) => result[1]);

function updateStats(update: Partial<ProviderApiPriceStats>, base = usage): ProviderUsage {
  return { ...base, api_price: { ...stats, ...update } };
}

test("GLM MiniMax xAI and OpenAI Codex plan backends display provider price totals", () => {
  for (const kind of ["glm_coding_plan", "minimax_coding_plan", "grok_plan", "openai_codex"]) equal(providerApiPriceSupported(kind), true);
  for (const kind of ["devin_plan", "kiro_plan", "deepseek_balance", "claude_code_plan", "nyro_gateway", "openai", "openai_api", undefined]) equal(providerApiPriceSupported(kind), false);
});

test("all supported providers show server-computed total, cycle price and full-cycle capacity", () => {
  for (const kind of ["glm_coding_plan", "minimax_coding_plan", "grok_plan", "openai_codex"]) {
    const html = render({ ...usage, kind });
    equal(values(html).join(","), "$228.79,$21.40,≈ $214.00");
    for (const label of ["Total API price", "Current-cycle API price", "Estimated cycle API capacity", "including failed and retried attempts",
      "clearing logs reduces these totals", "current reference rates", "not an upstream bill", "includes usage so far, not remaining balance", "Used for projection: 10%", "Quota window: Weekly"]) ok(html.includes(label));
    match(html, /data-provider-api-price="provider-a"/);
    doesNotMatch(html, /\$428\.00|\$2,287\.90|Total tokens|role="progressbar"/);
  }
  equal(stats.estimated_capacity?.[0].total_cost, 214); // No mutation or UI recalculation.
});

test("OpenAI Codex weekly-only quota uses the same three price metrics in both languages", () => {
  const codex = { ...usage, kind: "openai_codex", level: "pro", tiers: [
    { name: "weekly_limit", used_percent: 76, resets_at: new Date(end).toISOString() },
    { name: "feature:codex_bengalfox:weekly_limit", used_percent: 99, resets_at: new Date(end - 3600_000).toISOString() },
  ] };
  const value = updateStats({ used_percent: 76,
    period: { ...period, equivalent_costs: [{ currency: "USD", total_cost: 152, priced_request_count: 3 }] },
    estimated_capacity: [{ currency: "USD", total_cost: 200, priced_request_count: 3 }],
  }, codex);
  for (const locale of ["en-US", "zh-CN"] as const) {
    const html = render(value, { locale });
    equal(values(html).join(","), "$228.79,$152.00,≈ $200.00");
    match(html, locale === "zh-CN" ? /用于估算的已用比例：76%/ : /Used for projection: 76%/);
    match(html, locale === "zh-CN" ? /OpenAI Codex.*已识别的 5 小时周期|已识别的 5 小时周期.*OpenAI Codex/ : /OpenAI Codex use an identified 5h cycle/);
    match(html, locale === "zh-CN" ? /功能专用窗口和时长未知的窗口不用于本周期统计/ : /feature-specific and unknown-duration windows are not used for current-cycle statistics/);
    doesNotMatch(html, /Used for projection: 99%|用于估算的已用比例：99%/);
  }
});

test("English and Chinese labels explain actual reset cycle and projection scope", () => {
  const html = render(usage, { locale: "zh-CN" });
  for (const label of ["累计 API 价格", "本周期 API 价格", "本周期预估可用 API 价格", "按此供应商 ID", "含失败与重试",
    "而非自然周或自然月", "不合并不同额度窗口", "浏览器本地时间", "含开始，不含结束", "包含已用部分，并非剩余余额", "用于估算的已用比例：10%", "当前参考价", "非上游实际账单"]) ok(html.includes(label));
  doesNotMatch(html, /Total API price|Current-cycle API price|Estimated cycle API capacity/);
  for (const locale of ["en-US", "zh-CN"] as const) {
    const markup = render(usage, { locale });
    ok(markup.includes(new Date(start).toLocaleString(locale)));
    ok(markup.includes(new Date(end).toLocaleString(locale)));
  }
});

test("missing stats distinguish loading and unavailable from priced or unused zero", () => {
  for (const api_price of [undefined, null]) {
    const missing = { ...usage, api_price };
    equal(values(render(missing)).join(","), "—,—,—");
    match(render(missing), /API price totals unavailable/);
    match(render(missing, { loading: true }), /aria-busy="true"/);
    match(render(missing, { loading: true }), /Loading price totals…/);
    match(render(missing, { locale: "zh-CN" }), /API 金额统计暂不可用/);
    doesNotMatch(render(missing), /\$0\.00|Current cycle unknown/);
  }
});

test("unknown or invalid backend cycles keep total price but never invent current zero", () => {
  for (const invalid of [
    { period: null, period_start: null, period_end: null }, { period: null },
    { period_start: null }, { period_end: null }, { period_start: NaN }, { period_end: Infinity },
    { period_end: 1e20 }, { period_start: end }, { period_end: start }, { period_start: -1 },
  ]) {
    const html = render(updateStats(invalid));
    equal(values(html).join(","), "$228.79,—,—");
    match(html, /Current cycle unknown; refresh usage\./);
    doesNotMatch(html, /Invalid Date|NaN|Infinity/);
  }
});

test("cached cycle is start-inclusive end-exclusive and never rolls forward", () => {
  equal(values(render(usage, { now: start }))[1], "$21.40");
  equal(values(render(usage, { now: end - 1 }))[1], "$21.40");
  equal(values(render(usage, { now: start - 1 })).join(","), "$228.79,—,—");
  for (const now of [end, end + 1, end + 31 * 86_400_000]) {
    const html = render(usage, { now });
    equal(values(html).join(","), "$228.79,—,—");
    match(html, /Current cycle ended; refresh usage\./);
    match(render(usage, { now, locale: "zh-CN" }), /本周期已结束，请刷新用量。/);
  }
});

test("stale or failed refresh preserves recorded price but suppresses projection", () => {
  const html = render(usage, { stale: true });
  equal(values(html).join(","), "$228.79,$21.40,—");
  match(html, /Showing stale totals; refresh usage to estimate capacity/);
  match(render(usage, { stale: true, locale: "zh-CN" }), /当前为旧数据，暂不估算可用金额/);
});

test("projection requires a unique matching tier, finite positive ratio and priced cycle", () => {
  for (const used_percent of [null, 0, -1, 101, NaN, Infinity]) {
    equal(values(render(updateStats({ used_percent })))[2], "—");
  }
  for (const tiers of [[], [usage.tiers[0]], [usage.tiers[1], usage.tiers[1]],
    [{ ...usage.tiers[1], used_percent: 11 }], [{ ...usage.tiers[1], resets_at: null }],
    [{ ...usage.tiers[1], resets_at: new Date(end + 1).toISOString() }]]) {
    equal(values(render({ ...usage, tiers }))[2], "—");
  }
  for (const estimated_capacity of [null, [], [{ currency: "USD", total_cost: 1, priced_request_count: 0 }],
    ...[-1, NaN, Infinity].map((total_cost) => [{ currency: "USD", total_cost, priced_request_count: 1 }])]) {
    equal(values(render(updateStats({ estimated_capacity })))[2], "—");
  }
  equal(values(render(updateStats({ period: zero })))[2], "—");
  match(render(updateStats({ estimated_capacity: null })), /Estimate unavailable/);
});

test("missing prices stay unknown, no requests display zero and mixed prices remain native", () => {
  const unpriced = { ...period, equivalent_costs: [], unpriced_request_count: 3 };
  const unknown = render(updateStats({ all_time: unpriced, period: unpriced, estimated_capacity: null }));
  equal(values(unknown).join(","), "—,—,—");
  match(unknown, /3 attempts unpriced and excluded/);
  equal(values(render(updateStats({ all_time: zero, period: zero, estimated_capacity: null }))).join(","), "$0.00,$0.00,—");
  const costs = [{ currency: "USD", total_cost: 1, priced_request_count: 1 }, { currency: "CNY", total_cost: 7, priced_request_count: 1 }];
  const html = render(updateStats({ period: { ...period, equivalent_costs: costs, unpriced_request_count: 1 },
    estimated_capacity: costs.map((cost) => ({ ...cost, total_cost: cost.total_cost * 10 })) }));
  equal(values(html).join(","), "$228.79,$1.00 + ¥7.00 *,≈ $10.00 + ¥70.00 *");
  match(html, /text-amber-700/);
  match(html, /1 attempts unpriced and excluded/);
  doesNotMatch(html, /Converted to USD/);
});

test("tiny amounts and explicitly priced zero preserve account-pool formatter behavior", () => {
  for (const [total_cost, expected] of [[0.0001, "&lt;$0.01"], [0, "$0.00"]] as const) {
    const costs = [{ currency: "USD", total_cost, priced_request_count: 1 }];
    const html = render(updateStats({ period: { ...period, equivalent_costs: costs }, estimated_capacity: costs }));
    equal(values(html)[1], expected);
    equal(values(html)[2], `≈ ${expected}`);
  }
});

test("5h and monthly fallback and Grok date-only reset display the backend-selected cycle", () => {
  for (const [kind, period_tier, reset, label] of [
    ["minimax_coding_plan", "five_hour", new Date(end).toISOString(), "5h"],
    ["openai_codex", "five_hour", new Date(end).toISOString(), "5h"],
    ["grok_plan", "monthly", new Date(end).toISOString(), "Monthly"],
    ["grok_plan", "weekly_limit", "2030-01-07", "Weekly"],
  ]) {
    const period_end = Date.parse(reset);
    const period_start = period_tier === "five_hour" ? period_end - 5 * 3600_000
      : period_tier === "monthly" ? Date.parse("2029-12-07T13:48:02Z") : period_end - 7 * 86_400_000;
    const value = updateStats({ period_tier, period_start, period_end }, { ...usage, kind,
      tiers: [{ name: period_tier, used_percent: 10, resets_at: reset }] });
    const html = render(value, { now: period_end - 3600_000 });
    equal(values(html)[2], "≈ $214.00");
    match(html, new RegExp(`Quota window: ${label}`));
  }
});
