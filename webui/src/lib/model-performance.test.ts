import { deepEqual, equal, ok, throws } from "node:assert/strict";
import { test } from "node:test";
import {
  buildPerformanceRows, buildPerformanceHitIndex, filterPerformanceRows, groupPerformancePoints,
  layoutPerformanceLabels, PERFORMANCE_CHART, performanceColor, performancePoints,
  performanceTpsMaximum, pointCoordinates, readPerformanceResponse, visiblePerformanceSelection,
  type ModelPerformance, type PerformancePoint, type PerformanceResponse, type PerformanceStats,
} from "./model-performance";
import { EFFORT_TIERS, type Provider, type ProviderModelRatingProfile } from "./types";

const time = "2026-09-08T00:00:00.000Z";
const provider = (id: string, name = id, is_enabled = true): Provider => ({
  id, name, is_enabled, protocol: "openai-compatible", base_url: "http://example.invalid",
  use_proxy: false, fast_mode: false, created_at: time, updated_at: time,
});
function profile(provider_id = "p", upstream_model = "model", score: number | null = 70): ProviderModelRatingProfile {
  return { provider_id, upstream_model, common: score === null ? null : { score, updated_at: time },
    overrides: { low: null, medium: null, high: null, xhigh: null, max: null },
    effective: Object.fromEntries(EFFORT_TIERS.map((tier) => [tier, { status: score === null ? "unrated" : "rated", score, source: score === null ? "unrated" : "common", score_updated_at: score === null ? null : time }])) as ProviderModelRatingProfile["effective"], display_mode: "common" };
}
const stats = (average_tps: number | null = 42.123456, valid_tps_count = 5): PerformanceStats => ({
  average_tps, valid_tps_count, selected_request_count: 10,
  first_sample_at: valid_tps_count ? 1000 : null, last_sample_at: valid_tps_count ? 2000 : null,
});
const model = (p = profile()): ModelPerformance => ({ profile: p, mixed: stats(999),
  tiers: Object.fromEntries(EFFORT_TIERS.map((tier) => [tier, stats()])) as ModelPerformance["tiers"],
  unclassified_count: 2, untrusted_count: 1, status: "ready" });
const snapshot = (...models: ModelPerformance[]): PerformanceResponse => ({ as_of: 3000, window_start: 0, models });
function point(key: string, score: number, tps: number): PerformancePoint {
  return { key, pointId: `P${key}`, providerId: key, providerName: key, model: key, providerEnabled: true,
    score, tps, status: "ready", tier: "mixed", source: "common", scoreUpdatedAt: time,
    selectedRequestCount: 10, validTpsCount: 5, firstSampleAt: 1000, lastSampleAt: 2000,
    unclassifiedCount: 0, untrustedCount: 0, color: performanceColor(key) };
}
test("strict batch contract preserves server TPS, sample counts and diagnostics verbatim", () => {
  const data = snapshot(model());
  equal(readPerformanceResponse(data), data);
  const row = buildPerformanceRows(data, [provider("p")])[0];
  equal(row.tps, 999); equal(row.validTpsCount, 5); equal(row.selectedRequestCount, 10);
  equal(row.unclassifiedCount, 2); equal(row.untrustedCount, 1);
  equal(row.firstSampleAt, 1000); equal(row.lastSampleAt, 2000);
});
test("malformed batches and duplicate exact pair responses are errors, never missing/zero", () => {
  for (const invalid of [null, {}, { as_of: 1, window_start: 0, models: [{}] },
    snapshot({ ...model(), mixed: { ...stats(), average_tps: NaN } }),
    snapshot({ ...model(), mixed: { ...stats(), valid_tps_count: 11 } }),
    snapshot({ ...model(), mixed: { ...stats(), average_tps: 0 } }),
    snapshot(model(), model())]) throws(() => readPerformanceResponse(invalid));
});
test("statistics enforce ten-request bound and matching TPS/sample-time presence", () => {
  for (const mixed of [
    { ...stats(), selected_request_count: 11 }, { ...stats(), average_tps: null },
    { ...stats(), first_sample_at: null }, { ...stats(), last_sample_at: null },
    { ...stats(), first_sample_at: -1 }, { ...stats(), first_sample_at: 3000 },
    { ...stats(null, 0), average_tps: 50 }, { ...stats(null, 0), first_sample_at: 1000 },
  ]) throws(() => readPerformanceResponse(snapshot({ ...model(), mixed })));
  const empty = snapshot({ ...model(), mixed: stats(null, 0) });
  equal(readPerformanceResponse(empty), empty);
  for (const fields of [{ as_of: -1 }, { window_start: -1 }, { window_start: 3001 }]) {
    throws(() => readPerformanceResponse({ ...snapshot(model()), ...fields }));
  }
});
test("hidden hover/pins never dim visible points and restored filters recover the pin", () => {
  const visible = [point("01", 20, 30)], pinned = ["02"];
  deepEqual(visiblePerformanceSelection(visible, [], pinned), []);
  deepEqual(visiblePerformanceSelection(visible, ["02"], ["01"]), ["01"]);
  deepEqual(visiblePerformanceSelection(visible, ["01"], pinned), ["01"]);
  deepEqual(visiblePerformanceSelection([...visible, point("02", 30, 40)], [], pinned), ["02"]);
  deepEqual(pinned, ["02"]);
});
test("common-only creates one mixed point; exact zero is distinct from unrated", () => {
  const rows = buildPerformanceRows(snapshot(model(profile("a", "Model ", 0)), model(profile("a", "Model", 100)), model(profile("b", "missing", null))), [provider("a")]);
  equal(rows.length, 3);
  deepEqual(performancePoints(rows).map((p) => p.score).sort((a, b) => a - b), [0, 100]);
  equal(rows.find((p) => p.providerId === "b")?.status, "unrated");
  ok(rows.every((p) => p.tier === "mixed"));
});
test("server per-effort mode and effective fallback/source are authoritative, equal override stays explicit", () => {
  const p = profile(); p.display_mode = "per_effort";
  p.overrides.high = { score: 70, updated_at: time };
  p.effective.high = { status: "rated", score: 70, source: "override", score_updated_at: time };
  // A differing backend effective value must never be replaced with local common fallback.
  p.effective.low = { status: "rated", score: 39, source: "common", score_updated_at: time };
  const m = model(p); m.tiers.medium = stats(null, 0);
  const rows = buildPerformanceRows(readPerformanceResponse(snapshot(m)), [provider("p")]);
  equal(rows.length, 5); ok(rows.every((r) => r.tier !== "mixed"));
  equal(rows.find((r) => r.tier === "high")?.source, "override");
  equal(rows.find((r) => r.tier === "low")?.score, 39);
  equal(rows.find((r) => r.tier === "low")?.tps, 42.123456);
  equal(rows.find((r) => r.tier === "medium")?.status, "missing");
  equal(performancePoints(rows).length, 4); // no mixed fallback despite mixed TPS 999
});
test("override-only profiles keep five rows but skip missing scores, not the entire profile", () => {
  const p = profile("p", "override-only", null); p.display_mode = "per_effort";
  p.overrides.max = { score: 0, updated_at: time };
  p.effective.max = { status: "rated", score: 0, source: "override", score_updated_at: time };
  const rows = buildPerformanceRows(snapshot(model(p)), [provider("p", "Disabled", false)]);
  equal(performancePoints(rows).length, 1); equal(performancePoints(rows)[0].score, 0);
  equal(rows.filter((r) => r.status === "unrated").length, 4);
  equal(rows[0].providerEnabled, false);
});
test("explicit backend error remains an exact-pair row and never becomes TPS 0", () => {
  const rows = buildPerformanceRows(snapshot({ ...model(profile("absent", "Model / ", 90)), status: "error", error: "storage unavailable" }), []);
  equal(rows[0].error, "storage unavailable"); equal(rows[0].providerId, "absent"); equal(rows[0].model, "Model / ");
  equal(rows[0].status, "error"); equal(performancePoints(rows).length, 0);
});
test("full snapshot IDs are assigned before search/provider/tier filters and independent of names/input order", () => {
  const input = snapshot(model(profile("z", "model")), model(profile("a", "model")));
  const rows = buildPerformanceRows(input, [provider("z"), provider("a")]);
  const reverse = buildPerformanceRows(snapshot(...[...input.models].reverse()), [provider("a", "renamed"), provider("z")]);
  deepEqual(rows.map((r) => [r.key, r.pointId]), reverse.map((r) => [r.key, r.pointId]));
  const selected = filterPerformanceRows(rows, "model", "z", "mixed");
  equal(selected.length, 1); equal(selected[0].pointId, "P02");
  equal(filterPerformanceRows(rows, "P02", null, null)[0].key, selected[0].key);
});
test("Y ceiling floor200; above floor expand in50 multiples with exact-multiple headroom", () => {
  for (const max of [0, 0.01, 100, 199, 200]) equal(performanceTpsMaximum([{ tps: max }]), 200);
  for (const [max, expected] of [[200.01, 250], [249, 250], [250, 300], [299, 300], [300, 350], [1000, 1050]]) equal(performanceTpsMaximum([{ tps: max }]), expected);
  equal(performanceTpsMaximum([]), 200);
  const rows = [point("01", 40, 50), point("02", 50, 250)];
  equal(performanceTpsMaximum(performancePoints(rows)), 300);
  const filtered = performancePoints(filterPerformanceRows(rows, "P01", null, null));
  equal(performanceTpsMaximum(filtered), 200); equal(filtered[0].pointId, "P01");
});
test("scores0/100 and TPS ceiling keep full circles inside SVG viewport", () => {
  for (const p of [point("01", 0, 0.01), point("02", 100, 200)]) {
    const { x, y } = pointCoordinates(p, 200);
    ok(x - 9 >= 0 && x + 9 <= PERFORMANCE_CHART.width);
    ok(y - 9 >= 0 && y + 9 <= PERFORMANCE_CHART.height);
  }
});
test("coincident groups preserve all members and near-hit index returns actual coordinates, never centroid", () => {
  const data = [point("01", 50, 60), point("02", 50, 60), point("03", 51, 61), point("04", 90, 150)];
  const before = JSON.stringify(data);
  const groups = groupPerformancePoints(data, 200); equal(groups.length, 3); equal(groups[0].points.length, 2);
  const hit = buildPerformanceHitIndex(groups);
  deepEqual(hit(groups[0].x, groups[0].y).map((p) => [p.pointId, p.score, p.tps]), [["P01", 50, 60], ["P02", 50, 60], ["P03", 51, 61]]);
  equal(hit(0, 0).length, 0); equal(JSON.stringify(data), before);
  // Centers 21px apart have overlapping 14px hit targets; choosing either exposes both.
  const overlapping = groupPerformancePoints([point("05", 50, 60), point("06", 53, 60)], 200);
  equal(buildPerformanceHitIndex(overlapping)(overlapping[0].x, overlapping[0].y).length, 2);
});
test("dense numbered labels never collide; unavailable slots deliberately omitted, point coordinates stationary", () => {
  const data = Array.from({ length: 90 }, (_, i) => point(String(i + 1).padStart(2, "0"), 50 + i / 100, 60 + i / 100));
  const before = JSON.stringify(data); const groups = groupPerformancePoints(data, 200);
  const labels = layoutPerformanceLabels(groups); ok(labels.size > 0); ok(labels.size < groups.length);
  const boxes = [...labels.values()];
  for (let i = 0; i < boxes.length; i++) for (let j = i + 1; j < boxes.length; j++) {
    const a = boxes[i], b = boxes[j];
    ok(a.x + a.width <= b.x || b.x + b.width <= a.x || a.y + a.height <= b.y || b.y + b.height <= a.y);
  }
  equal(JSON.stringify(data), before); deepEqual(layoutPerformanceLabels(groups), labels);
});
