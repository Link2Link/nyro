import { deepEqual, doesNotMatch, equal, match, ok } from "node:assert/strict";
import { test } from "node:test";
import type { ReactElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { AccountUsageTotals, ProviderApiPriceStats, ProviderUsage, QuotaBucket, QuotaBucketFilter } from "../lib/types";
import { ProviderQuotaBuckets, QuotaBucketAvailability, QuotaBucketSourceLinks } from "./provider-quota-buckets";

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
const usage: ProviderUsage = { provider_id: "real-provider", kind: "other_subscription", site: "global", queried_at: new Date(now).toISOString(),
  tiers: [{ name: "weekly_limit", used_percent: 99 }], quota_buckets: [flash, argon],
  // Poison aggregate cycle values: they must never appear in either child or provider total.
  api_price: { ...price(439, 999, 9999, now + 7 * 86400_000, 99), unassigned: totals(8, 42) },
  balances: [{ currency: "USD", total: 888, topped_up: 888, granted: 0 }],
};
const render = (value = usage, stale = false, locale: "en-US" | "zh-CN" = "en-US") => renderToStaticMarkup(
  <ProviderQuotaBuckets usage={value} now={now} stale={stale} loading={false} locale={locale} onDetails={() => {}} />);
const articles = (html: string) => [...html.matchAll(/<article\b[^]*?<\/article>/g)].map((value) => value[0]);
const values = (html: string) => [...html.matchAll(/<dd[^>]*>(.*?)<\/dd>/g)].map((value) => value[1]);
const familySource = "google_family_display_v1";
const variants = (model: string) => [model, ...["high", "medium", "low"].map((effort) => `${model}-${effort}`)];
const rawFlash: QuotaBucket = { ...flash, id: "raw-flash", label: "Original Flash allowance", models: variants("gemini-3.8-flash"),
  binding: { ...flash.binding, status: "window_constraint" }, constraint_ids: [] };
const rawArgon: QuotaBucket = { ...argon, id: "raw-argon", label: "Original Argon allowance", models: variants("gemini-4-argon"),
  binding: { ...argon.binding, status: "window_constraint" } };
const familyFlash: QuotaBucket = { ...flash, id: "display-family-flash", label: "gemini-3.8-flash", models: rawFlash.models,
  binding: { ...flash.binding, source: familySource }, source: familySource, constraint_ids: [rawFlash.id] };
const familyArgon: QuotaBucket = { ...argon, id: "display-family-argon", label: "gemini-4-argon", models: rawArgon.models,
  binding: { ...argon.binding, source: familySource }, source: familySource, constraint_ids: [rawArgon.id] };
const googleUsage: ProviderUsage = { ...usage, kind: "google_subscription", quota_buckets: [familyFlash, familyArgon, rawFlash, rawArgon] };

/** Only content outside a closed details element is visible on the compact card. */
function withoutDetails(html: string): string {
  let depth = 0;
  return html.split(/(<details\b[^>]*>|<\/details>)/g).filter((part) => {
    if (/^<details\b/.test(part)) { depth++; return false; }
    if (part === "</details>") { depth--; return false; }
    return depth === 0;
  }).join("");
}

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

test("Google displays only backend family cards with all effort names inside closed diagnostics", () => {
  const coverage = { input_tokens: 100, output_tokens: 50, cache_read_tokens: 20, missing_usage_request_count: 1,
    cache_read_reported_request_count: 1, priced_request_count: 2, unpriced_request_count: 1, attributed_request_count: 2,
    inferred_request_count: 1, unassigned_request_count: 0, legacy_timestamp_request_count: 0 };
  const detailedFlash = { ...familyFlash, api_price: { ...familyFlash.api_price!, all_time_coverage: coverage,
    models: familyFlash.models.map((model) => ({ model, all_time: totals(1), coverage })) } };
  const unrelated = { ...rawArgon, id: "other-model-resource", label: "Other model allowance", models: ["other-model"] };
  // Even legacy primary-looking observations are diagnostics in the Google structured view.
  const unmarked = { ...argon, id: "unmarked-model", label: "Unmarked allowance" };
  const html = render({ ...googleUsage, quota_buckets: [detailedFlash, familyArgon, rawFlash, rawArgon, unrelated, unmarked] });
  const [first, second] = articles(html);
  equal(articles(html).length, 2);
  equal(values(first).join(","), "$184.20,$24.00,≈ $40.00");
  equal(values(second).join(","), "$246.80,$18.00,≈ $60.00");
  equal((html.match(/data-api-price="all_time"/g) ?? []).length, 3);
  equal((html.match(/data-api-price="period"/g) ?? []).length, 2);
  equal((html.match(/role="progressbar"/g) ?? []).length, 4);
  equal((html.match(/data-bucket-diagnostics=/g) ?? []).length, 2);
  match(first, /Exact-model price breakdown|Missing usage/);
  match(html, /<details data-provider-quota-diagnostics=/);
  doesNotMatch(html, /<details[^>]*\bopen(?:=|\s|>)/);
  const compact = withoutDetails(html);
  for (const model of [...familyFlash.models, ...familyArgon.models]) {
    ok(html.includes(model));
    if (model.endsWith("-high") || model.endsWith("-medium") || model.endsWith("-low")) ok(!compact.includes(model));
  }
  doesNotMatch(compact, /Unmarked allowance|Other model allowance|Original Flash allowance|Exact-model price breakdown|Missing usage|Google family display/);
  match(compact, />\$439\.00</);
  doesNotMatch(compact, /Unassigned details/);
  match(html, /Unassigned details/);
  const chinese = render(googleUsage, false, "zh-CN");
  match(chinese, /模型与诊断详情|配额诊断与历史|原始归属桶详情/);
});

test("Google missing family observations never create zero cards or fall back to raw or parent capacity", () => {
  for (const quota_buckets of [[], [rawFlash, rawArgon], [{ ...flash, id: "unmarked" }]]) {
    const html = render({ ...googleUsage, quota_buckets });
    equal(articles(html).length, 0);
    equal((html.match(/No quota observations for the target model families/g) ?? []).length, 1);
    equal(values(html).join(","), "$439.00");
    doesNotMatch(withoutDetails(html), /role="progressbar"|\$0\.00|99% used|estimated_capacity|≈/);
  }
  const one = render({ ...googleUsage, quota_buckets: [familyArgon, rawArgon] });
  equal(articles(one).length, 1);
  match(articles(one)[0], /gemini-4-argon/);
  doesNotMatch(one, /display-family-flash|\$0\.00/);
  equal(render({ ...googleUsage, quota_buckets: undefined }), "");
});

test("Google family availability counts only observed families within one owner and never claims missing siblings are healthy", () => {
  const status = (buckets: QuotaBucket[]) => renderToStaticMarkup(<QuotaBucketAvailability buckets={buckets} now={now} expectedCount={2} />);
  match(status([familyFlash, familyArgon, rawFlash, rawArgon]), /Partially available · 1\/2/);
  match(status([familyArgon]), /data-quota-availability="partial"[^]*1\/2/);
  match(status([familyFlash]), /data-quota-availability="unknown"/);
  match(status([rawFlash, rawArgon]), /data-quota-availability="unknown"/);
  match(status([]), /data-quota-availability="unknown"/);
  match(status([{ ...familyFlash, scheduling: familyArgon.scheduling }, familyArgon]), /data-quota-availability="available"/);
  match(status([familyFlash, { ...familyArgon, scheduling: familyFlash.scheduling }]), /data-quota-availability="exhausted"/);
  for (const buckets of [[familyArgon, { ...familyArgon, scope_id: "owner-b" }],
    [familyFlash, familyArgon, { ...familyArgon, scope_id: "owner-b" }]]) {
    const html = status(buckets);
    match(html, /data-quota-availability="unknown"/);
    doesNotMatch(html, /Available|Partially available|\d\/\d/);
  }
});

test("family source detail actions use only meaningful same-scope raw identities, never synthetic IDs", () => {
  const shared = { ...rawFlash, id: "shared", binding: { ...rawFlash.binding, status: "shared_constraint" } };
  const noModels = { ...rawFlash, id: "window-only", models: [] };
  const family = { ...familyFlash, constraint_ids: [rawFlash.id, shared.id, noModels.id, familyArgon.id, "missing"] };
  const resources = [rawFlash, { ...rawFlash, scope_id: "owner-b", label: "Other owner" }, shared, noModels, familyArgon];
  const selections: QuotaBucketFilter[] = [];
  const element = QuotaBucketSourceLinks({ bucket: family, resources, onDetails: (filter) => selections.push(filter) });
  ok(element);
  type SourceItem = ReactElement<{ children: ReactElement<{ onClick: (event: { stopPropagation: () => void }) => void }> }>;
  const list = element.props.children[2] as ReactElement<{ children: SourceItem[] }>;
  for (const item of list.props.children) item.props.children.props.onClick({ stopPropagation() {} });
  deepEqual(selections, [{ quota_bucket: rawFlash.id, quota_scope: rawFlash.scope_id }]);
  const html = renderToStaticMarkup(element);
  match(html, /data-quota-detail="\[&quot;owner-a&quot;,&quot;raw-flash&quot;\]"/);
  doesNotMatch(html, /display-family-|Other owner|window-only/);
  equal(QuotaBucketSourceLinks({ bucket: { ...family, constraint_ids: [] }, resources, onDetails() {} }), null);
  const card = articles(render({ ...googleUsage, quota_buckets: [familyArgon] }))[0];
  doesNotMatch(card, /Bucket details|data-quota-detail|data-quota-source-links/);
});

test("Google family prices retain recorded totals without stale, missing or mismatched capacity projections", () => {
  const stale = articles(render({ ...googleUsage, quota_buckets: [{ ...familyFlash, stale: true }, familyArgon] }));
  equal(values(stale[0]).join(","), "$184.20,$24.00,—");
  equal(values(stale[1]).join(","), "$246.80,$18.00,≈ $60.00");
  for (const family of [{ ...familyArgon, api_price: null, tiers: [{ name: "weekly_limit", used_percent: null as unknown as number }] },
    { ...familyArgon, api_price: { ...familyArgon.api_price!, used_percent: 99 } }]) {
    const card = articles(render({ ...googleUsage, quota_buckets: [familyFlash, family] }))[1];
    equal(values(card).join(","), family.api_price ? "$246.80,$18.00,—" : "—,—,—");
    doesNotMatch(card, /\$0\.00|NaN|Infinity|\$184\.20/);
  }
});

test("current raw attribution stays current while retired owner history and unassigned details remain collapsed", () => {
  const coverage = { input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, missing_usage_request_count: 0,
    cache_read_reported_request_count: 0, priced_request_count: 0, unpriced_request_count: 0, attributed_request_count: 0,
    inferred_request_count: 0, unassigned_request_count: 0, legacy_timestamp_request_count: 0 };
  const retained = { scope_id: rawFlash.scope_id, bucket_id: rawFlash.id, all_time: totals(184.2), coverage, models: [] };
  const html = render({ ...googleUsage, api_price: { ...googleUsage.api_price!, retained_buckets: [retained,
    { ...retained, scope_id: "retired-owner", all_time: totals(9) }] } });
  equal((html.match(/History details/g) ?? []).length, 1);
  match(html, /Retired bucket history \(not current capacity\) · 1/);
  match(html, /\$9\.00/);
  doesNotMatch(withoutDetails(html), /Retired bucket history|History details|Unassigned details|\$9\.00/);
});

test("same Google family and raw IDs in different scopes keep cards, prices and source links isolated", () => {
  const otherRaw = { ...rawFlash, scope_id: "owner-b", api_price: price(55, 5, 50, now + 3 * 86400_000, 10) };
  const otherFamily = { ...familyFlash, scope_id: "owner-b", scheduling: familyArgon.scheduling, api_price: otherRaw.api_price,
    tiers: familyFlash.tiers.map((tier) => ({ ...tier, used_percent: tier.name === "weekly_limit" ? 10 : 5 })) };
  const html = render({ ...googleUsage, quota_buckets: [familyFlash, otherFamily, rawFlash, otherRaw] });
  const cards = articles(html);
  equal(cards.length, 2);
  const cardKeys = [...html.matchAll(/data-quota-bucket="([^"]*)"/g)].map((value) => value[1]);
  equal(new Set(cardKeys).size, 2);
  const windows = [...html.matchAll(/data-bucket-window="([^"]*)"/g)].map((value) => value[1]);
  equal(new Set(windows).size, 4);
  equal(values(cards[0]).join(","), "$184.20,$24.00,≈ $40.00");
  equal(values(cards[1]).join(","), "$55.00,$5.00,≈ $50.00");
  match(cards[0], /data-quota-detail="\[&quot;owner-a&quot;,&quot;raw-flash&quot;\]"/);
  match(cards[1], /data-quota-detail="\[&quot;owner-b&quot;,&quot;raw-flash&quot;\]"/);
  doesNotMatch(cards[1], /\$184\.20|owner-a/);
});
