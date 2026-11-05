import { doesNotMatch, equal, match, ok } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import type { AccountUsageTotals, ProviderApiPriceStats, ProviderUsage, QuotaBucket } from "../lib/types";
import { ProviderQuotaBuckets, QuotaBucketAvailability } from "./provider-quota-buckets";

const now = Date.parse("2030-01-01T00:00:00Z");
const totals = (price: number, requests = 3): AccountUsageTotals => ({ total_tokens: 123456, request_count: requests,
  equivalent_costs: [{ currency: "USD", total_cost: price, priced_request_count: requests }], unpriced_request_count: 0 });
const price = (all: number, cycle: number, capacity: number, end: number, used: number): ProviderApiPriceStats => ({
  all_time: totals(all), period: totals(cycle), period_start: end - 7 * 86400_000, period_end: end,
  period_tier: "weekly_limit", used_percent: used, estimated_capacity: [{ currency: "USD", total_cost: capacity, priced_request_count: 3 }],
});
const flash: QuotaBucket = {
  id: "flash", scope_id: "owner-a", label: "Gemini Flash", models: ["gemini-3.8-flash", "gemini-3.8-flash-high"],
  binding: { status: "resolved", source: "configured", revision: 2 }, source: "enforced_summary",
  queried_at: new Date(now).toISOString(), stale: false, constraint_ids: ["shared"],
  scheduling: { status: "quota_exhausted", reason: "usage_limit", blocking_tiers: ["five_hour"] },
  tiers: [{ name: "five_hour", used_percent: 100, resets_at: new Date(now + 3600_000).toISOString() },
    { name: "weekly_limit", used_percent: 60, resets_at: new Date(now + 3 * 86400_000).toISOString() }],
  api_price: price(184.2, 24, 40, now + 3 * 86400_000, 60),
};
const argon: QuotaBucket = {
  ...flash, id: "argon", label: "Gemini Argon", models: ["gemini-4-argon"], constraint_ids: [],
  scheduling: { status: "eligible", blocking_tiers: [] },
  tiers: [{ name: "five_hour", used_percent: 18, resets_at: new Date(now + 2 * 3600_000).toISOString() },
    { name: "weekly_limit", used_percent: 30, resets_at: new Date(now + 5 * 86400_000).toISOString() }],
  api_price: price(246.8, 18, 60, now + 5 * 86400_000, 30),
};
const usage: ProviderUsage = { provider_id: "real-provider", kind: "google_subscription", site: "global", queried_at: new Date(now).toISOString(),
  tiers: [{ name: "weekly_limit", used_percent: 99 }], quota_buckets: [flash, argon],
  // Poison aggregate cycle values: they must never appear in either child or provider total.
  api_price: { ...price(439, 999, 9999, now + 7 * 86400_000, 99), unassigned: totals(8, 42) },
  balances: [{ currency: "USD", total: 888, topped_up: 888, granted: 0 }],
};
const render = (value = usage, stale = false, locale: "en-US" | "zh-CN" = "en-US") => renderToStaticMarkup(
  <ProviderQuotaBuckets usage={value} now={now} stale={stale} loading={false} locale={locale} onDetails={() => {}} />);
const articles = (html: string) => [...html.matchAll(/<article\b[^]*?<\/article>/g)].map((value) => value[0]);
const values = (html: string) => [...html.matchAll(/<dd[^>]*>(.*?)<\/dd>/g)].map((value) => value[1]);

test("one provider contains independently priced Flash and Argon subcards with one grand total", () => {
  const html = render();
  const [first, second] = articles(html);
  equal(articles(html).length, 2);
  equal(values(first).join(","), "$184.20,$24.00,≈ $40.00");
  equal(values(second).join(","), "$246.80,$18.00,≈ $60.00");
  equal(values(html).join(","), "$184.20,$24.00,≈ $40.00,$246.80,$18.00,≈ $60.00,$439.00");
  equal((html.match(/>\$439\.00</g) ?? []).length, 1);
  equal((html.match(/data-api-price="period"/g) ?? []).length, 2);
  equal((html.match(/data-api-price="estimated_capacity"/g) ?? []).length, 2);
  match(first, /routing paused/);
  match(second, /data-bucket-status="available"/);
  doesNotMatch(second, /routing paused|\$184\.20|\$24\.00/);
  doesNotMatch(html, /\$999|\$9,999|888|99% used/);
  match(html, /Unassigned[^]*\$8\.00[^]*42 attempts/);
  match(html, /independent bucket cycles and capacities are not combined/);
  match(html, /Bucket details/);
  match(html, /Unassigned details/);
  equal((html.match(/role="progressbar"/g) ?? []).length, 4);
});

test("provider availability summarizes backend decisions without a combined percentage", () => {
  const renderStatus = (buckets: QuotaBucket[]) => renderToStaticMarkup(<QuotaBucketAvailability buckets={buckets} now={now} />);
  const partial = renderStatus([flash, argon]);
  match(partial, /data-quota-availability="partial"/);
  match(partial, /Partially available · 1\/2/);
  doesNotMatch(partial, /%|progressbar/);
  match(renderStatus([argon, { ...flash, scheduling: argon.scheduling }]), /data-quota-availability="available"/);
  match(renderStatus([flash, { ...argon, scheduling: flash.scheduling }]), /data-quota-availability="exhausted"/);
  match(renderStatus([{ ...argon, stale: true }]), /data-quota-availability="unknown"/);
  match(renderStatus([{ ...argon, binding: { ...argon.binding, status: "unresolved" } }]), /data-quota-availability="unknown"/);
  match(renderStatus([{ ...argon, source: "catalog" }]), /data-quota-availability="unknown"/);
  for (const reason of ["unknown_mapping", "conflicting_binding"]) {
    match(renderStatus([{ ...argon, scheduling: { ...argon.scheduling, reason } }]), /data-quota-availability="unknown"/);
  }
});

test("same weekly names and bucket IDs across owner scopes retain compound identities", () => {
  const duplicateId = { ...argon, id: flash.id, scope_id: "owner-b" };
  const html = render({ ...usage, quota_buckets: [flash, duplicateId] });
  const windows = [...html.matchAll(/data-bucket-window="([^"]*)"/g)].map((value) => value[1]);
  equal(windows.length, 4);
  equal(new Set(windows).size, 4);
  equal((html.match(/Weekly used quota/g) ?? []).length, 2);
  // No reset timestamp, mutable label or percentage participates in a key.
  const renamed = render({ ...usage, quota_buckets: [{ ...flash, label: "Renamed", tiers: flash.tiers.map((tier) => ({ ...tier, used_percent: 1 })) }, duplicateId] });
  equal([...renamed.matchAll(/data-bucket-window="([^"]*)"/g)].map((value) => value[1]).join(), windows.join());
});

test("partial refresh keeps exhausted evidence and suppresses only the stale bucket projection", () => {
  const html = render({ ...usage, quota_buckets: [{ ...flash, stale: true }, argon] });
  const [first, second] = articles(html);
  match(first, /routing paused/);
  match(first, /stale data does not prove recovery/);
  equal(values(first).join(","), "$184.20,$24.00,—");
  equal(values(second).join(","), "$246.80,$18.00,≈ $60.00");
  doesNotMatch(render(usage, true), /≈ \$/);
  const aged = render({ ...usage, quota_buckets: [{ ...argon, queried_at: new Date(now - 5 * 60_000).toISOString() }] });
  equal(values(articles(aged)[0]).join(","), "$246.80,$18.00,—");
  match(aged, /data-bucket-status="unknown"/);
});

test("missing prices, null utilization and unknown cycles never borrow a sibling's capacity", () => {
  const unknown = { ...argon, api_price: null, binding: { ...argon.binding, status: "unresolved" },
    tiers: [{ name: "weekly_limit", used_percent: null as unknown as number, resets_at: null }] };
  const html = render({ ...usage, quota_buckets: [flash, unknown] });
  const second = articles(html)[1];
  equal(values(second).join(","), "—,—,—");
  match(second, /data-bucket-status="unknown"/);
  match(second, /Usage unknown/);
  doesNotMatch(second, /aria-valuenow="0"|\$0\.00|\$184\.20|NaN|Infinity/);
  const noCycle = render({ ...usage, quota_buckets: [{ ...argon, api_price: { ...argon.api_price!, period: null, period_start: null, period_end: null } }] });
  equal(values(articles(noCycle)[0]).join(","), "$246.80,—,—");
});

test("shared and consumed window resources never become ordinary capacity alternatives", () => {
  for (const status of ["shared_constraint", "window_constraint"]) {
    const resource = { ...argon, id: "resource", label: "Global resource", binding: { ...argon.binding, status } };
    const html = render({ ...usage, quota_buckets: [flash, argon, resource] });
    equal(articles(html).length, 2);
    const summary = renderToStaticMarkup(<QuotaBucketAvailability buckets={[flash, resource]} now={now} />);
    match(summary, /data-quota-availability="exhausted"/);
    doesNotMatch(summary, /1\/2|partial/);
  }
});

test("unresolved or catalog observations never project leftover backend capacity", () => {
  for (const bucket of [{ ...argon, source: "catalog" }, { ...argon, binding: { ...argon.binding, status: "unresolved" } },
    { ...argon, scheduling: { ...argon.scheduling, reason: "unknown_mapping" } }]) {
    const html = render({ ...usage, quota_buckets: [bucket] });
    equal(values(articles(html)[0]).join(","), "$246.80,$18.00,—");
    match(html, /data-bucket-status="unknown"/);
  }
});

test("explicit Chinese labels and empty buckets preserve the English canonical fallback", () => {
  const html = render(usage, false, "zh-CN");
  for (const label of ["保留请求", "已记录 Token", "累计 API 价格", "本周期 API 价格", "未归属", "查看桶详情"]) ok(html.includes(label));
  equal(render({ ...usage, quota_buckets: [] }), "");
  equal(render({ ...usage, quota_buckets: undefined }), "");
});
