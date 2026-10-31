import { doesNotMatch, equal, match, ok } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import type { AccountUsageStats, AccountUsageTotals } from "../lib/types";
import { AccountUsageSummary } from "./account-usage-summary";

const zero: AccountUsageTotals = { total_tokens: 0, request_count: 0, equivalent_costs: [], unpriced_request_count: 0 };
const priced: AccountUsageTotals = { total_tokens: 1_234_567, request_count: 3, equivalent_costs: [{ currency: "USD", total_cost: 12.5, priced_request_count: 3 }], unpriced_request_count: 0 };
const periodStart = Date.parse("2026-10-21T15:24:37Z");
const periodEnd = Date.parse("2026-10-28T15:24:37Z");
const stats: AccountUsageStats = { account_id: "stable-account-id", all_time: priced, period: zero, period_start: periodStart, period_end: periodEnd };
const props = { now: periodStart + 86_400_000, loading: false, error: false, locale: "en-US" as const };
const render = (value?: AccountUsageStats, overrides: Partial<Parameters<typeof AccountUsageSummary>[0]> = {}) =>
  renderToStaticMarkup(<AccountUsageSummary {...props} stats={value} {...overrides} />);

// Only the four metric values; the surrounding labels/tooltips intentionally carry context.
function values(html: string) { return [...html.matchAll(/<span class="[^"]*text-xs font-semibold[^>]*>(.*?)<\/span>/g)].map((match) => match[1]); }

test("account metrics show retained and backend-defined current-cycle logs", () => {
  const html = render(stats);
  match(html, /data-account-usage="stable-account-id"/);
  match(html, />All records</);
  match(html, />Current cycle</);
  match(html, />Total tokens</);
  match(html, />Equivalent API price</);
  equal(values(html).join(","), "1.23M,$12.50,0,$0.00");
  match(html, /including failed and retried attempts/);
  match(html, /not upstream quota or a lifetime bill/);
  match(html, /actual reset cycle, not a calendar week or month/);
  match(html, /start inclusive, end exclusive/);
  match(html, /current reference rates/);
  doesNotMatch(html, />Requests<|>3<|Input tokens|Output tokens|Cache tokens|This week|This month|refresh usage/);
});

test("cycle tooltips preserve both backend timestamps in browser local time", () => {
  const previous = process.env.TZ;
  process.env.TZ = "America/New_York";
  try {
    for (const locale of ["en-US", "zh-CN"] as const) {
      const html = render(stats, { locale });
      ok(html.includes(new Date(periodStart).toLocaleString(locale)));
      ok(html.includes(new Date(periodEnd).toLocaleString(locale)));
    }
  } finally {
    if (previous === undefined) delete process.env.TZ;
    else process.env.TZ = previous;
  }
});

test("unknown and invalid cycles retain all-time values but show dashes and a visible refresh hint", () => {
  for (const cycle of [
    { period: null, period_start: null, period_end: null },
    { period: null, period_start: periodStart, period_end: periodEnd },
    { period: priced, period_start: null, period_end: periodEnd },
    { period: priced, period_start: periodStart, period_end: null },
    { period: priced, period_start: Number.NaN, period_end: periodEnd },
    { period: priced, period_start: periodStart, period_end: Number.POSITIVE_INFINITY },
    { period: priced, period_start: periodStart, period_end: 1e20 },
    { period: priced, period_start: periodEnd, period_end: periodStart },
  ]) {
    const html = render({ ...stats, ...cycle });
    equal(values(html).join(","), "1.23M,$12.50,—,—");
    match(html, /role="status"[^>]*>Current cycle unknown; refresh usage\./);
    doesNotMatch(html, /Invalid Date|NaN|Infinity/);
  }
});

test("cached cycles are valid at start but expire at end without rolling old totals into a new cycle", () => {
  for (const now of [periodStart, periodEnd - 1]) {
    equal(values(render(stats, { now })).join(","), "1.23M,$12.50,0,$0.00");
  }
  equal(values(render(stats, { now: periodStart - 1 })).join(","), "1.23M,$12.50,—,—");
  for (const now of [periodEnd, periodEnd + 1, periodEnd + 31 * 86_400_000]) {
    for (const error of [false, true]) {
      const html = render(stats, { now, error });
      equal(values(html).join(","), "1.23M,$12.50,—,—");
      match(html, /role="status"[^>]*>Current cycle ended; refresh usage\./);
    }
  }
  equal(stats.period, zero); // Display expiry does not mutate the shared query cache.
});

test("unpriced requests are not free while genuinely unused cycles display zero", () => {
  const missing = render({ ...stats, all_time: { ...priced, equivalent_costs: [], unpriced_request_count: 3 } });
  equal(values(missing).join(","), "1.23M,—,0,$0.00");
  match(missing, /3 attempts unpriced and excluded/);
  match(missing, /No reference price/);
  const partial = render({ ...stats, all_time: { ...priced, unpriced_request_count: 2 } });
  equal(values(partial)[1], "$12.50 *");
  match(partial, /2 attempts unpriced and excluded/);
  match(partial, /text-amber-700/);
  // No usable pricing buckets must not become a zero just because unpriced_count is zero.
  equal(values(render({ ...stats, all_time: { ...priced, equivalent_costs: [] } }))[1], "—");
});

test("native API-price currencies remain separate instead of silently converting or adding", () => {
  const html = render({ ...stats, all_time: { ...priced, equivalent_costs: [
    { currency: "USD", total_cost: 1, priced_request_count: 1 },
    { currency: "CNY", total_cost: 70, priced_request_count: 1 },
  ] } });
  equal(values(html)[1], "$1.00 + ¥70.00");
  match(html, /USD: Equivalent API price/);
  match(html, /CNY: Equivalent API price/);
  doesNotMatch(html, /Converted to USD|\$11\.00/);
});

test("tiny prices and explicitly priced zeroes retain existing formatter semantics", () => {
  const html = render({ ...stats, all_time: { ...priced, equivalent_costs: [{ currency: "USD", total_cost: 0.001, priced_request_count: 1 }] },
    period: { ...priced, equivalent_costs: [{ currency: "USD", total_cost: 0, priced_request_count: 1 }] } });
  equal(values(html).join(","), "1.23M,&lt;$0.01,1.23M,$0.00");
});

test("loading, unavailable and failed responses never render zero totals", () => {
  for (const [overrides, message] of [
    [{ loading: true }, /Loading totals…/],
    [{ error: true }, /Usage totals unavailable/],
    [{}, /Account totals unavailable/],
  ] as const) {
    const html = render(undefined, overrides);
    equal(values(html).join(","), "—,—,—,—");
    match(html, message);
    equal((html.match(/role="status"/g) ?? []).length, 1);
    doesNotMatch(html, /\$0\.00|>0<|Current cycle unknown|refresh usage/);
  }
  match(render(undefined, { loading: true }), /aria-busy="true"/);
});

test("failed background refresh keeps cached totals only within the current cycle and marks them stale", () => {
  const html = render(stats, { error: true });
  equal(values(html).join(","), "1.23M,$12.50,0,$0.00");
  match(html, /role="status"[^>]*>Refresh failed · showing stale totals/);
  doesNotMatch(html, /Usage totals unavailable/);
});

test("Kiro token estimate warning uses an explicit prop, even with an unknown cycle", () => {
  doesNotMatch(render(stats), /Kiro tokens/);
  match(render(stats, { tokensEstimated: true }), /Kiro tokens are estimates, not actual credit usage/);
  match(render({ ...stats, period: null, period_start: null, period_end: null }, { tokensEstimated: true }), /Kiro tokens are estimates, not actual credit usage/);
  match(render(stats, { locale: "zh-CN", tokensEstimated: true }), /Kiro Token 为估算值，不代表实际 Credit 用量/);
});

test("Chinese cycle metrics localize labels, reset scope, pricing gaps and refresh hints", () => {
  const html = render({ ...stats, all_time: { ...priced, unpriced_request_count: 1 } }, { locale: "zh-CN", error: true });
  for (const label of ["全部记录", "本周期", "总 Token", "等价 API 价格", "按账号实际重置周期", "而非自然周或自然月", "浏览器本地时间", "含开始，不含结束", "含失败与重试", "1 次尝试未计价，未计入", "刷新失败 · 当前为旧数据"]) match(html, new RegExp(label));
  doesNotMatch(html, /This week|This month|Current cycle|All records|Refresh failed/);
  match(render({ ...stats, period: null, period_start: null, period_end: null }, { locale: "zh-CN" }), /role="status"[^>]*>本周期未知，请刷新用量。/);
  match(render(stats, { locale: "zh-CN", now: periodEnd }), /role="status"[^>]*>本周期已结束，请刷新用量。/);
});
