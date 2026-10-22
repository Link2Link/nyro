import { doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import type { AccountUsageStats, AccountUsageTotals } from "../lib/types";
import { AccountUsageSummary } from "./account-usage-summary";

const zero: AccountUsageTotals = { total_tokens: 0, request_count: 0, equivalent_costs: [], unpriced_request_count: 0 };
const priced: AccountUsageTotals = { total_tokens: 1_234_567, request_count: 3, equivalent_costs: [{ currency: "USD", total_cost: 12.5, priced_request_count: 3 }], unpriced_request_count: 0 };
const stats: AccountUsageStats = { account_id: "stable-account-id", all_time: priced, period: zero };
const props = { period: "week" as const, periodStart: new Date(2026, 9, 19).getTime(), loading: false, error: false, locale: "en-US" as const };
const render = (value?: AccountUsageStats, overrides: Partial<Parameters<typeof AccountUsageSummary>[0]> = {}) =>
  renderToStaticMarkup(<AccountUsageSummary {...props} stats={value} {...overrides} />);

// Only the four metric values; the surrounding labels/tooltips intentionally carry context.
function values(html: string) { return [...html.matchAll(/<span class="[^"]*text-xs font-semibold[^>]*>(.*?)<\/span>/g)].map((match) => match[1]); }

test("account metrics show only token and API-price totals for retained and calendar-period logs", () => {
  const html = render(stats);
  match(html, /data-account-usage="stable-account-id"/);
  match(html, />All records</);
  match(html, />This week</);
  match(html, />Total tokens</);
  match(html, />Equivalent API price</);
  equal(values(html).join(","), "1.23M,$12.50,0,$0.00");
  match(html, /including failed and retried attempts/);
  match(html, /not upstream quota or a lifetime bill/);
  match(html, /calendar week starts Monday/);
  match(html, /current reference rates/);
  doesNotMatch(html, />Requests<|>3<|Input tokens|Output tokens|Cache tokens/);
});

test("unpriced requests are not free while genuinely unused periods display zero", () => {
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
    doesNotMatch(html, /\$0\.00|>0</);
  }
  match(render(undefined, { loading: true }), /aria-busy="true"/);
});

test("failed background refresh keeps cached totals and visibly marks them stale", () => {
  const html = render(stats, { error: true });
  equal(values(html).join(","), "1.23M,$12.50,0,$0.00");
  match(html, /role="status"[^>]*>Refresh failed · showing stale totals/);
  doesNotMatch(html, /Usage totals unavailable/);
});

test("Chinese monthly metrics localize labels, calendar scope, pricing gaps and failures", () => {
  const html = render({ ...stats, all_time: { ...priced, unpriced_request_count: 1 } }, { locale: "zh-CN", period: "month", error: true });
  for (const label of ["全部记录", "本月", "总 Token", "等价 API 价格", "自然月从 1 号开始", "浏览器本地时区", "含失败与重试", "1 次尝试未计价，未计入", "刷新失败 · 当前为旧数据"]) match(html, new RegExp(label));
  doesNotMatch(html, /This week|This month|All records|Refresh failed/);
});
