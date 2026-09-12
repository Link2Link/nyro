import { deepEqual, equal, ok, throws } from "node:assert/strict";
import { test } from "node:test";
import {
  buildPerformanceRows, buildPerformanceHitIndex, buildPerformanceEnvelope, envelopeLabelGroups, filterPerformanceRows, groupPerformancePoints,
  layoutPerformanceLabels, PERFORMANCE_CHART, performanceColor, performanceModelLabel, performancePoints,
  PERFORMANCE_MARKER_SIZE, performanceTpsMaximum, performanceScoreDomain, performanceScoreMaximum, performanceXSpan,
  pointCoordinates, readPerformanceResponse, scoreX, visiblePerformanceSelection,
  type ModelPerformanceItem, type PerformancePoint, type PerformanceResponse, type PerformanceStats,
} from "./model-performance";
import type { Provider } from "./types";
import { resolveProviderIconKey } from "./provider-icon-resolve";

const time = "2026-09-08T00:00:00.000Z";
const provider = (id: string, name = id, is_enabled = true, identity: Partial<Provider> = {}): Provider => ({
  id, name, is_enabled, protocol: "openai-compatible", base_url: "http://example.invalid",
  use_proxy: false, fast_mode: false, created_at: time, updated_at: time, ...identity,
});const stats = (average_tps: number | null = 42.123456, valid_tps_count = 5): PerformanceStats => ({
  average_tps, valid_tps_count, selected_request_count: 10,
  first_sample_at: valid_tps_count ? 1000 : null, last_sample_at: valid_tps_count ? 2000 : null,
});
const variant = (upstream_model = "model", mixed = stats(999)) => ({
  upstream_model, mixed, unclassified_count: 1, untrusted_count: 1,
});
const item = (model_prefix = "model", provider_id = "p", score = 70): ModelPerformanceItem => ({
  model_prefix, provider_id, score, score_updated_at: time, mixed: stats(999),
  variants: [variant(model_prefix)], unclassified_count: 2, untrusted_count: 1,
});
const snapshot = (...models: ModelPerformanceItem[]): PerformanceResponse => ({ as_of: 3000, window_start: null, models });
function point(key: string, score: number, tps: number): PerformancePoint {
  return { key, pointId: `P${key}`, providerId: key, providerName: key,
    providerIcon: "", providerBaseUrl: "http://example.invalid",
    modelPrefix: key, model: key, providerEnabled: true,
    score, tps, overallTps: tps, netTps: tps, status: "ready", scoreUpdatedAt: time,
    selectedRequestCount: 10, validTpsCount: 5, firstSampleAt: 1000, lastSampleAt: 2000,
    unclassifiedCount: 0, untrustedCount: 0, variants: [], color: performanceColor(key) };
}
test("strict batch contract preserves server TPS, sample counts, variants and diagnostics verbatim", () => {
  const data = snapshot(item());
  equal(readPerformanceResponse(data), data);
  const row = buildPerformanceRows(data, [provider("p")])[0];
  equal(row.tps, 999); equal(row.validTpsCount, 5); equal(row.selectedRequestCount, 10);
  equal(row.unclassifiedCount, 2); equal(row.untrustedCount, 1);
  equal(row.firstSampleAt, 1000); equal(row.lastSampleAt, 2000);
  equal(row.variants.length, 1); equal(row.variants[0].upstream_model, "model");
  equal(row.status, "ready"); equal(row.score, 70);
});
test("rows carry the provider identity the icon marker resolves, and stay empty when unknown", () => {
  const known = buildPerformanceRows(snapshot(item()), [provider("p", "DeepSeek Relay", false)])[0];
  equal(known.providerName, "DeepSeek Relay"); equal(known.providerEnabled, false);
  equal(known.providerIcon, ""); equal(known.providerBaseUrl, "http://example.invalid");
  const unknown = buildPerformanceRows(snapshot(item()), [])[0];
  equal(unknown.providerName, "p"); equal(unknown.providerEnabled, false);
  equal(unknown.providerIcon, ""); equal(unknown.providerBaseUrl, "");
});
test("marker identity is the preset key, then the vendor, and never the wire protocol", () => {
  // Shipped-icon predicate over the real catalog subset these cases touch.
  const shipped = new Set(["nyro", "bailian", "doubao", "gemini", "openai", "deepseek", "kimi"]);
  const icon = (name: string, identity: Partial<Provider> = {}) => {
    const row = buildPerformanceRows(snapshot(item()), [provider("p", name, true, identity)])[0];
    return resolveProviderIconKey(
      { iconKey: row.providerIcon || undefined, name: row.providerName, baseUrl: row.providerBaseUrl },
      (key) => shipped.has(key),
    );
  };
  // The canonical identity wins, through the aliases the vendor metadata declares.
  equal(icon("阿里百炼", { vendor: "bailian" }), "bailian");
  equal(icon("火山引擎", { preset_key: "ark-coding" }), "doubao");
  // Without a preset the name still identifies a branded provider.
  equal(icon("Beta Gemini"), "gemini");
  // A relay with no identity must never inherit the OpenAI mark from its request
  // format: the wire protocol is not a brand.
  equal(icon("Some Relay"), null);
  equal(icon("apinebula"), null);
  // A relay the app marks as `custom` has no shipped vector mark, so its own name and
  // host decide, and an unidentified one stays unresolved rather than borrowing a brand.
  equal(icon("apinebula", { preset_key: "custom", vendor: "custom" }), null);
  equal(icon("UUAPI gemini", { preset_key: "custom", vendor: "custom" }), "gemini");
});
test("malformed batches and duplicate prefix/provider groups are errors, never missing/zero", () => {
  for (const invalid of [null, {}, { as_of: 1, window_start: 0, models: [{}] },
    snapshot({ ...item(), mixed: { ...stats(), average_tps: NaN } }),
    snapshot({ ...item(), mixed: { ...stats(), valid_tps_count: 11 } }),
    snapshot({ ...item(), mixed: { ...stats(), average_tps: 0 } }),
    snapshot(item(), item()),
    snapshot({ ...item(), score: 101 }),
    snapshot({ ...item(), model_prefix: "" }),
    snapshot({ ...item(), variants: [{ ...variant(), upstream_model: "" }] }),
    snapshot({ ...item(), variants: [{ ...variant(), mixed: { ...stats(), valid_tps_count: 11 } }] })]) {
    throws(() => readPerformanceResponse(invalid));
  }
});
test("variant statistics enforce the ten-request window and matching TPS/sample-time presence", () => {
  for (const mixed of [
    { ...stats(), selected_request_count: 11 }, { ...stats(), average_tps: null },
    { ...stats(), first_sample_at: null }, { ...stats(), last_sample_at: null },
    { ...stats(), first_sample_at: -1 }, { ...stats(), first_sample_at: 3000 },
    { ...stats(null, 0), average_tps: 50 }, { ...stats(null, 0), first_sample_at: 1000 },
  ]) throws(() => readPerformanceResponse(snapshot({ ...item(), mixed })));
  const empty = snapshot({ ...item(), mixed: stats(null, 0) });
  equal(readPerformanceResponse(empty), empty);
  for (const fields of [{ as_of: -1 }, { window_start: -1 }, { window_start: 3001 }]) {
    throws(() => readPerformanceResponse({ ...snapshot(item()), ...fields }));
  }
});
test("merged group statistics are bounded by their variant count, not by one variant window", () => {
  const merged = (selected_request_count: number, valid_tps_count: number): PerformanceStats => ({
    selected_request_count, valid_tps_count, average_tps: valid_tps_count ? 53.0750848124811 : null,
    first_sample_at: valid_tps_count ? 1000 : null, last_sample_at: valid_tps_count ? 2000 : null,
  });
  const group = (mixed: PerformanceStats) => ({ ...item(),
    variants: [variant("model"), variant("model-0813")], mixed });
  // Two variants, each retaining a full ten-call window, merge into twenty samples.
  const live = snapshot(group(merged(20, 20)));
  equal(readPerformanceResponse(live), live);
  // A group without any usable TPS still merges its selected samples.
  const unrated = snapshot(group(merged(20, 0)));
  equal(readPerformanceResponse(unrated), unrated);
  // Merging is a sum: counts above ten are valid, counts above ten per variant are not.
  for (const mixed of [merged(21, 20), merged(20, 21), merged(21, 21)]) {
    throws(() => readPerformanceResponse(snapshot(group(mixed))));
  }
  const partial = snapshot(group(merged(20, 10)));
  equal(readPerformanceResponse(partial), partial);
  // A group with no variants was merged from nothing, so it may carry no samples at all.
  const none = snapshot({ ...item(), variants: [], mixed: merged(0, 0) });
  equal(readPerformanceResponse(none), none);
  throws(() => readPerformanceResponse(snapshot({ ...item(), variants: [], mixed: merged(1, 0) })));
  // The bound follows the declared variants, so a single-variant group keeps the ten-call window.
  throws(() => readPerformanceResponse(snapshot({ ...item(), mixed: merged(11, 11) })));
});
test("hidden hover/pins never dim visible points and restored filters recover the pin", () => {
  const visible = [point("01", 20, 30)], pinned = ["02"];
  deepEqual(visiblePerformanceSelection(visible, [], pinned), []);
  deepEqual(visiblePerformanceSelection(visible, ["02"], ["01"]), ["01"]);
  deepEqual(visiblePerformanceSelection(visible, ["01"], pinned), ["01"]);
  deepEqual(visiblePerformanceSelection([...visible, point("02", 30, 40)], [], pinned), ["02"]);
  deepEqual(pinned, ["02"]);
});
test("one point per prefix × provider including zero scores", () => {
  const rows = buildPerformanceRows(
    snapshot(item("m", "a", 0), item("m", "b", 100), item("m2", "a", 50)),
    [provider("a"), provider("b")],
  );
  equal(rows.length, 3);
  deepEqual(performancePoints(rows).map((p) => p.score).sort((a, b) => a - b), [0, 50, 100]);
  ok(rows.every((p) => p.tps === 999));
  deepEqual(rows.map((row) => [row.modelPrefix, row.providerId]), [["m", "a"], ["m", "b"], ["m2", "a"]]);
});
test("missing mixed TPS stays missing and disabled suppliers retain their score", () => {
  const m = { ...item(), mixed: stats(null, 0) };
  const rows = buildPerformanceRows(readPerformanceResponse(snapshot(m)), [provider("p", "Disabled", false)]);
  equal(rows.length, 1); equal(rows[0].status, "missing");
  equal(rows[0].score, 70); equal(rows[0].providerEnabled, false);
  equal(performancePoints(rows).length, 0);
});
test("legacy rating-keyed shapes cannot masquerade as prefix groups", () => {
  throws(() => readPerformanceResponse({ ...snapshot(), models: [{ rating: {}, mixed: stats() }] } as unknown as PerformanceResponse));
});
test("full snapshot IDs are assigned before search/provider filters and independent of names/input order", () => {
  const input = snapshot(item("model", "z"), item("model", "a"));
  const rows = buildPerformanceRows(input, [provider("z"), provider("a")]);
  const reverse = buildPerformanceRows(snapshot(...[...input.models].reverse()), [provider("a", "renamed"), provider("z")]);
  deepEqual(rows.map((r) => [r.key, r.pointId]), reverse.map((r) => [r.key, r.pointId]));
  const selected = filterPerformanceRows(rows, "model", "z");
  equal(selected.length, 1); equal(selected[0].pointId, "P02");
  equal(filterPerformanceRows(rows, "P02", null).length, 0);
  // Search reaches concrete variant names too.
  const variants = snapshot({ ...item("m", "z", 80), variants: [variant("deepseek-v4-pro-0813")] });
  equal(filterPerformanceRows(buildPerformanceRows(variants, [provider("z")]), "0813", null).length, 1);
});
test("point labels always pair the prefix with the provider", () => {
  equal(performanceModelLabel(point("01", 50, 60)), "01 · 01");
  const a = { ...point("01", 50, 60), modelPrefix: "shared-prefix", providerName: "Alpha" };
  equal(performanceModelLabel(a), "shared-prefix · Alpha");
});
test("envelope handles empty, single, dominating and two trade-off points", () => {
  deepEqual(buildPerformanceEnvelope([]).nodes, []);
  const a = point("a", 40, 200), b = point("b", 90, 100), dominant = point("d", 100, 250);
  deepEqual(buildPerformanceEnvelope([a]).nodes.map((n) => n.memberKeys), [["a"]]);
  deepEqual(buildPerformanceEnvelope([a, b]).nodes.map((n) => [n.score, n.tps]), [[40, 200], [90, 100]]);
  deepEqual([...buildPerformanceEnvelope([a, b, dominant]).memberKeys], ["d"]);
});
test("envelope skips Pareto dents but retains outer bends and collinear members", () => {
  const a = point("a", 40, 200), c = point("c", 90, 100);
  deepEqual(buildPerformanceEnvelope([a, point("dent", 60, 120), c]).nodes.map((n) => n.memberKeys), [["a"], ["c"]]);
  deepEqual(buildPerformanceEnvelope([a, point("bend", 60, 180), c]).nodes.map((n) => n.memberKeys), [["a"], ["bend"], ["c"]]);
  deepEqual(buildPerformanceEnvelope([a, point("on-line", 60, 160), c]).nodes.map((n) => n.memberKeys), [["a"], ["on-line"], ["c"]]);
});
test("envelope tie handling preserves every exact duplicate but no dominated horizontal/vertical edges", () => {
  const points = [point("slower", 40, 180), point("a", 40, 200), point("weaker", 30, 200),
    point("b", 90, 100), point("copy-b", 90, 100), point("slower-b", 90, 80)];
  const result = buildPerformanceEnvelope(points);
  deepEqual(result.nodes.map((n) => [n.score, n.tps]), [[40, 200], [90, 100]]);
  deepEqual([...result.memberKeys].sort(), ["a", "b", "copy-b"]);
  deepEqual([...buildPerformanceEnvelope([point("a", 40, 10), point("b", 40, 20)]).memberKeys], ["b"]);
  deepEqual([...buildPerformanceEnvelope([point("a", 40, 20), point("b", 50, 20)]).memberKeys], ["b"]);
});
test("envelope uses raw TPS, includes hollow points, is deterministic and never mutates input", () => {
  const points = [point("a", 40, 200), { ...point("b", 90, 100.01), validTpsCount: 1 }, point("slower", 90, 100)];
  const before = JSON.stringify(points), result = buildPerformanceEnvelope(points);
  deepEqual([...result.memberKeys], ["a", "b"]);
  deepEqual(buildPerformanceEnvelope([...points].reverse()), result);
  equal(JSON.stringify(points), before);
  // This is measurably below the line, not a tolerance based on displayed decimal digits.
  ok(!buildPerformanceEnvelope([points[0], point("dent", 60, 160 - 1e-9), point("end", 90, 100)]).memberKeys.has("dent"));
  const decimal = buildPerformanceEnvelope([point("a", 0, 0.3), point("b", 50, 0.2), point("c", 100, 0.1)]);
  ok(decimal.memberKeys.has("b"), "floating arithmetic must retain collinear decimal evidence");
});
test("envelope recomputes on visible valid data and projects with the existing adaptive axes", () => {
  const a = point("a", 40, 200), b = point("b", 60, 120), c = point("c", 90, 100);
  const points = [a, b, c];
  ok(!buildPerformanceEnvelope(points).memberKeys.has("b"));
  ok(buildPerformanceEnvelope(points.filter((p) => p !== c)).memberKeys.has("b"));
  const invalid = [{ ...point("bad", 100, 999), status: "missing" as const, tps: null }];
  equal(performancePoints([...points, ...invalid] as unknown as PerformancePoint[]).length, 3);
  const domain = performanceScoreDomain(points), yMax = performanceTpsMaximum(points);
  for (const node of buildPerformanceEnvelope(points).nodes) {
    const original = points.find((p) => p.key === node.memberKeys[0])!;
    deepEqual(pointCoordinates(node, yMax, domain), pointCoordinates(original, yMax, domain));
  }
});
test("every envelope supporting segment has all input points at or below it", () => {
  let seed = 42;
  const next = () => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed; };
  for (let sample = 0; sample < 40; sample++) {
    const points = Array.from({ length: 50 }, (_, i) => point(String(i), next() % 101, 1 + next() % 10000));
    const { nodes } = buildPerformanceEnvelope(points);
    for (let i = 1; i < nodes.length; i++) {
      const a = nodes[i - 1], b = nodes[i];
      ok(b.score > a.score && b.tps < a.tps);
      for (const p of points) {
        const cross = (b.score - a.score) * (p.tps - a.tps) - (b.tps - a.tps) * (p.score - a.score);
        ok(cross <= 1e-7, `point ${p.key} lies above supporting segment`);
      }
    }
  }
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
test("X domain snaps to visible minimum and maximum score ticks", () => {
  deepEqual(performanceScoreDomain([]), { min: 0, max: 100 });
  for (const [score, domain] of [
    [0, { min: 0, max: 10 }], [1, { min: 0, max: 10 }], [10, { min: 10, max: 20 }],
    [11, { min: 10, max: 20 }], [53, { min: 50, max: 60 }], [60, { min: 60, max: 70 }],
    [78, { min: 70, max: 80 }], [99, { min: 90, max: 100 }], [100, { min: 90, max: 100 }],
  ] as const) deepEqual(performanceScoreDomain([{ score }]), domain);
  const points = [point("01", 53, 50), point("02", 78, 80)];
  deepEqual(performanceScoreDomain(points), { min: 50, max: 80 });
  equal(performanceScoreMaximum(points), 80);
  const filtered = performancePoints(filterPerformanceRows(points, "01", null));
  const domain = performanceScoreDomain(filtered);
  deepEqual(domain, { min: 50, max: 60 });
  const groups = groupPerformancePoints(filtered, 100, domain);
  const span = performanceXSpan();
  equal(groups[0].x, span.left + (span.right - span.left) * 3 / 10);
  equal(groups[0].points[0].score, 53);
  equal(groups[0].points[0].pointId, "P01");
  equal(pointCoordinates(point("03", 60, 50), 100, domain).x, span.right);
  equal(pointCoordinates(point("04", 50, 50), 100, domain).x, span.left);
});
test("extreme scores keep marker room from the axes instead of straddling them", () => {
  // The margin covers half a marker plus air, so the box of the leftmost/rightmost
  // visible score never reaches the Y axis or the right frame.
  const domain = { min: 30, max: 60 };
  const span = performanceXSpan();
  const half = PERFORMANCE_MARKER_SIZE / 2;
  for (const score of [30, 60]) {
    const x = scoreX(score, domain);
    ok(x - half >= PERFORMANCE_CHART.left, `score ${score} marker stays right of the axis`);
    ok(x + half <= PERFORMANCE_CHART.right, `score ${score} marker stays left of the frame`);
  }
  ok(span.left - PERFORMANCE_CHART.left > half, "Margin exceeds the marker half-width");
  // Ticks, points and the envelope share one mapping, so nothing drifts off the ticks.
  equal(scoreX(45, domain), pointCoordinates(point("05", 45, 10), 100, domain).x);
  equal(scoreX(domain.min, domain), span.left);
  equal(scoreX(domain.max, domain), span.right);
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
test("only envelope positions receive direct labels; interior and dominated groups stay hover-only", () => {
  const a = point("a", 40, 200), bend = point("bend", 60, 180), c = point("c", 90, 100), dent = point("dent", 60, 120);
  const envelope = buildPerformanceEnvelope([a, bend, c, dent]);
  deepEqual([...envelope.memberKeys].sort(), ["a", "bend", "c"]);
  const labeled = envelopeLabelGroups(groupPerformancePoints([a, bend, c, dent], 200), envelope);
  deepEqual(labeled.map((group) => group.key).sort(),
    [a, bend, c].map((p) => JSON.stringify([p.score, p.tps])).sort(),
    "only boundary positions get direct labels, the Pareto dent none");
  // Coincident boundary models share one position and one labeled group.
  const copy = point("copy-c", 90, 100);
  const groups = groupPerformancePoints([a, dent, c, copy], 200);
  const envelope2 = buildPerformanceEnvelope([a, dent, c, copy]);
  const labeled2 = envelopeLabelGroups(groups, envelope2);
  deepEqual(labeled2.map((group) => group.key).sort(), [a, c, copy].map((p) => JSON.stringify([p.score, p.tps])).filter((key, index, keys) => keys.indexOf(key) === index).sort());
  ok(labeled2.every((group) => group.points.every((p) => envelope2.memberKeys.has(p.key))));
  const coincident = labeled2.find((group) => group.key === JSON.stringify([90, 100]))!;
  deepEqual(coincident.points.map((p) => p.key).sort(), ["c", "copy-c"]);
  // A singleton boundary (no dashed line is drawn) still labels its point: it is the outer boundary.
  const single = envelopeLabelGroups(groupPerformancePoints([dent], 200), buildPerformanceEnvelope([dent]));
  equal(single.length, 1); deepEqual(single[0].points, [dent]);
  // Empty data labels nothing.
  deepEqual(envelopeLabelGroups([], buildPerformanceEnvelope([])), []);
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
test("labels list every coincident point and pair the prefix with each provider", () => {
  const a = { ...point("01", 50, 60), modelPrefix: "shared-prefix", providerName: "Alpha" };
  const b = { ...point("02", 50, 60), modelPrefix: "shared-prefix", providerName: "Beta" };
  const groups = groupPerformancePoints([a, b], 200);
  const label = layoutPerformanceLabels(groups).get(groups[0].key)!;
  ok(label.lines.join("").includes("shared-prefix · Alpha"));
  ok(label.lines.join("").includes("shared-prefix · Beta"));
  ok(label.width <= 210); ok(label.lines.length >= 2);
  const long = { ...point("03", 10, 100), modelPrefix: "模型/very-long-".repeat(5) };
  const longGroups = groupPerformancePoints([long], 200);
  const wrapped = layoutPerformanceLabels(longGroups).get(longGroups[0].key)!;
  equal(wrapped.lines.join(""), `${long.modelPrefix} · 03`); ok(wrapped.lines.length > 1);
});

test("overall_tps contract preserves aggregate values and supports overall metric points", () => {
  const customStats: PerformanceStats = {
    average_tps: 80.0,
    overall_tps: 45.0,
    total_output_tokens: 450,
    total_latency_ms: 10000,
    selected_request_count: 10,
    valid_tps_count: 5,
    first_sample_at: 1000,
    last_sample_at: 2000,
  };
  const testItem: ModelPerformanceItem = {
    ...item("test-model", "p", 85),
    mixed: customStats,
    variants: [{ upstream_model: "test-model", mixed: customStats, unclassified_count: 0, untrusted_count: 0 }],
  };
  const data = snapshot(testItem);
  equal(readPerformanceResponse(data), data);
  const rows = buildPerformanceRows(data, [provider("p")]);
  equal(rows[0].tps, 80.0);
  equal(rows[0].overallTps, 45.0);

  // Net points
  const netPoints = performancePoints(rows, "net");
  equal(netPoints.length, 1);
  equal(netPoints[0].tps, 80.0);
  equal(netPoints[0].metric, "net");

  // Overall points
  const overallPoints = performancePoints(rows, "overall");
  equal(overallPoints.length, 1);
  equal(overallPoints[0].tps, 45.0);
  equal(overallPoints[0].metric, "overall");
});

