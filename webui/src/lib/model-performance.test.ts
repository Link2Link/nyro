import { deepEqual, equal, ok, throws } from "node:assert/strict";
import { test } from "node:test";
import {
  buildPerformanceRows, buildPerformanceHitIndex, filterPerformanceRows, groupPerformancePoints,
  layoutPerformanceLabels, PERFORMANCE_CHART, performanceColor, performancePoints,
  performanceTpsMaximum, performanceScoreMaximum, pointCoordinates, readPerformanceResponse, visiblePerformanceSelection,
  type ModelPerformance, type PerformancePoint, type PerformanceResponse, type PerformanceStats,
} from "./model-performance";
import type { Provider, ProviderModelRating } from "./types";

const time = "2026-09-08T00:00:00.000Z";
const provider = (id: string, name = id, is_enabled = true): Provider => ({
  id, name, is_enabled, protocol: "openai-compatible", base_url: "http://example.invalid",
  use_proxy: false, fast_mode: false, created_at: time, updated_at: time,
});
function rating(provider_id = "p", upstream_model = "model", score = 70): ProviderModelRating {
  return { provider_id, upstream_model, score, updated_at: time };
}
const stats = (average_tps: number | null = 42.123456, valid_tps_count = 5): PerformanceStats => ({
  average_tps, valid_tps_count, selected_request_count: 10,
  first_sample_at: valid_tps_count ? 1000 : null, last_sample_at: valid_tps_count ? 2000 : null,
});
const model = (p = rating()): ModelPerformance => ({ rating: p, mixed: stats(999),
  unclassified_count: 2, untrusted_count: 1, status: "ready" });
const snapshot = (...models: ModelPerformance[]): PerformanceResponse => ({ as_of: 3000, window_start: null, models });
function point(key: string, score: number, tps: number): PerformancePoint {
  return { key, pointId: `P${key}`, providerId: key, providerName: key, model: key, providerEnabled: true,
    score, tps, status: "ready", scoreUpdatedAt: time,
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
test("one point per exact pair uses the comprehensive score including zero", () => {
  const rows = buildPerformanceRows(snapshot(model(rating("a", "Model ", 0)), model(rating("a", "Model", 100))), [provider("a")]);
  equal(rows.length, 2);
  deepEqual(performancePoints(rows).map((p) => p.score).sort((a, b) => a - b), [0, 100]);
  ok(rows.every((p) => p.tps === 999));
});
test("missing mixed TPS stays missing and disabled suppliers retain their score", () => {
  const m = { ...model(rating()), mixed: stats(null, 0) };
  const rows = buildPerformanceRows(readPerformanceResponse(snapshot(m)), [provider("p", "Disabled", false)]);
  equal(rows.length, 1); equal(rows[0].status, "missing");
  equal(rows[0].score, 70); equal(rows[0].providerEnabled, false);
  equal(performancePoints(rows).length, 0);
});
test("legacy tier profiles cannot masquerade as a single rating", () => {
  throws(() => readPerformanceResponse(snapshot({ ...model(), rating: null } as unknown as ModelPerformance)));
  throws(() => readPerformanceResponse({ ...snapshot(), models: [{ profile: rating(), mixed: stats() }] }));
});
test("explicit backend error remains an exact-pair row and never becomes TPS 0", () => {
  const rows = buildPerformanceRows(snapshot({ ...model(rating("absent", "Model / ", 90)), status: "error", error: "storage unavailable" }), []);
  equal(rows[0].error, "storage unavailable"); equal(rows[0].providerId, "absent"); equal(rows[0].model, "Model / ");
  equal(rows[0].status, "error"); equal(performancePoints(rows).length, 0);
});
test("full snapshot IDs are assigned before search/provider filters and independent of names/input order", () => {
  const input = snapshot(model(rating("z", "model")), model(rating("a", "model")));
  const rows = buildPerformanceRows(input, [provider("z"), provider("a")]);
  const reverse = buildPerformanceRows(snapshot(...[...input.models].reverse()), [provider("a", "renamed"), provider("z")]);
  deepEqual(rows.map((r) => [r.key, r.pointId]), reverse.map((r) => [r.key, r.pointId]));
  const selected = filterPerformanceRows(rows, "model", "z");
  equal(selected.length, 1); equal(selected[0].pointId, "P02");
  equal(filterPerformanceRows(rows, "P02", null).length, 0);
});
test("Y ceiling defaults to100; above100 expand in50 multiples with headroom", () => {
  for (const max of [0, 0.01, 99.99, 100]) equal(performanceTpsMaximum([{ tps: max }]), 100);
  for (const [max, expected] of [[100.01, 150], [149, 150], [150, 200], [199, 200], [200, 250], [225, 250], [250, 300], [1000, 1050]]) equal(performanceTpsMaximum([{ tps: max }]), expected);
  equal(performanceTpsMaximum([]), 100);
  const rows = [point("01", 40, 50), point("02", 50, 250)];
  equal(performanceTpsMaximum(performancePoints(rows)), 300);
  const filtered = performancePoints(filterPerformanceRows(rows, "01", null));
  equal(performanceTpsMaximum(filtered), 100); equal(filtered[0].pointId, "P01");
});
test("X ceiling follows visible scores with ten-point ticks and a safe empty/zero domain", () => {
  equal(performanceScoreMaximum([]), 100);
  for (const [score, max] of [[0, 10], [1, 10], [10, 10], [11, 20], [53, 60], [60, 60], [78, 80], [99, 100], [100, 100]]) {
    equal(performanceScoreMaximum([{ score }]), max);
  }
  const points = [point("01", 53, 50), point("02", 78, 80)];
  equal(performanceScoreMaximum(points), 80);
  const filtered = performancePoints(filterPerformanceRows(points, "01", null));
  const max = performanceScoreMaximum(filtered);
  equal(max, 60);
  const groups = groupPerformancePoints(filtered, 100, max);
  equal(groups[0].x, PERFORMANCE_CHART.left + (PERFORMANCE_CHART.right - PERFORMANCE_CHART.left) * 53 / 60);
  equal(groups[0].points[0].score, 53);
  equal(groups[0].points[0].pointId, "P01");
  equal(pointCoordinates(point("03", 60, 50), 100, 60).x, PERFORMANCE_CHART.right);
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
test("dense model labels never collide; unavailable slots deliberately omitted, point coordinates stationary", () => {
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
test("labels list every coincident model and disambiguate same models by provider", () => {
  const a = { ...point("01", 50, 60), model: "shared-model", providerName: "Alpha" };
  const b = { ...point("02", 50, 60), model: "shared-model", providerName: "Beta" };
  const groups = groupPerformancePoints([a, b], 200);
  const label = layoutPerformanceLabels(groups).get(groups[0].key)!;
  ok(label.lines.join("").includes("shared-model · Alpha"));
  ok(label.lines.join("").includes("shared-model · Beta"));
  ok(label.width <= 210); ok(label.lines.length >= 2);
  const long = { ...point("03", 10, 100), model: "模型/very-long-".repeat(5) };
  const longGroups = groupPerformancePoints([long], 200);
  const wrapped = layoutPerformanceLabels(longGroups).get(longGroups[0].key)!;
  equal(wrapped.lines.join(""), long.model); ok(wrapped.lines.length > 1);
});
