import type { Provider } from "./types";

export interface PerformanceStats {
  selected_request_count: number;
  valid_tps_count: number;
  average_tps: number | null;
  overall_tps?: number | null;
  total_output_tokens?: number;
  total_latency_ms?: number;
  first_sample_at: number | null;
  last_sample_at: number | null;
}
export interface ModelPerformanceVariant {
  upstream_model: string;
  mixed: PerformanceStats;
  unclassified_count: number;
  untrusted_count: number;
}
/** One point: a rated prefix at one provider, variants merged by sample weight. */
export interface ModelPerformanceItem {
  model_prefix: string;
  provider_id: string;
  score: number;
  score_updated_at: string;
  mixed: PerformanceStats;
  variants: ModelPerformanceVariant[];
  unclassified_count: number;
  untrusted_count: number;
}
export interface PerformanceResponse { as_of: number; window_start: number | null; models: ModelPerformanceItem[] }
/** The backend retains at most ten calls per upstream variant. */
const PERFORMANCE_SAMPLE_LIMIT = 10;
const count = (value: unknown) => typeof value === "number" && Number.isInteger(value) && value >= 0;
const finite = (value: unknown) => typeof value === "number" && Number.isFinite(value);
/** A group's merged statistics sum its variants, so their bound is not the per-variant one. */
function isStats(value: unknown, maxSelected: number): value is PerformanceStats {
  if (!value || typeof value !== "object") return false;
  const v = value as Record<string, unknown>;
  if (!count(v.selected_request_count) || Number(v.selected_request_count) > maxSelected
    || !count(v.valid_tps_count) || Number(v.valid_tps_count) > Number(v.selected_request_count)) return false;
  if (v.overall_tps !== undefined && v.overall_tps !== null && (!finite(v.overall_tps) || Number(v.overall_tps) <= 0)) {
    return false;
  }
  if (v.valid_tps_count === 0) return v.average_tps === null && v.first_sample_at === null && v.last_sample_at === null;
  return finite(v.average_tps) && Number(v.average_tps) > 0
    && finite(v.first_sample_at) && Number(v.first_sample_at) >= 0
    && finite(v.last_sample_at) && Number(v.last_sample_at) >= Number(v.first_sample_at);
}
function isVariant(value: unknown): value is ModelPerformanceVariant {
  if (!value || typeof value !== "object") return false;
  const v = value as Record<string, unknown>;
  return typeof v.upstream_model === "string" && v.upstream_model.length > 0
    && isStats(v.mixed, PERFORMANCE_SAMPLE_LIMIT) && count(v.unclassified_count) && count(v.untrusted_count);
}
/** Validate the batch as a contract, not as invented zero-valued statistics. */
export function readPerformanceResponse(value: unknown): PerformanceResponse {
  const fail = () => { throw new Error("Invalid model performance response. Please check backend compatibility."); };
  if (!value || typeof value !== "object") return fail();
  const v = value as Record<string, unknown>;
  if (!finite(v.as_of) || Number(v.as_of) < 0
    || (v.window_start !== null && (!finite(v.window_start) || Number(v.window_start) < 0 || Number(v.window_start) > Number(v.as_of)))
    || !Array.isArray(v.models)) return fail();
  const keys = new Set<string>();
  for (const item of v.models) {
    if (!item || typeof item !== "object") return fail();
    const model = item as Record<string, unknown>;
    if (typeof model.model_prefix !== "string" || !model.model_prefix
      || typeof model.provider_id !== "string" || !model.provider_id
      || !Number.isInteger(model.score) || Number(model.score) < 0 || Number(model.score) > 100
      || typeof model.score_updated_at !== "string"
      || !Array.isArray(model.variants) || !model.variants.every(isVariant)
      || !count(model.unclassified_count) || !count(model.untrusted_count)) return fail();
    // Merged counts are the sum over this group's variants, so the bound scales with them.
    if (!isStats(model.mixed, PERFORMANCE_SAMPLE_LIMIT * model.variants.length)) return fail();
    const key = JSON.stringify([model.model_prefix, model.provider_id]);
    if (keys.has(key)) return fail();
    keys.add(key);
  }
  return value as PerformanceResponse;
}
export interface PerformanceRowVariant {
  upstream_model: string;
  stats: PerformanceStats;
}
export interface PerformanceRow {
  key: string;
  pointId: string;
  modelPrefix: string;
  providerId: string;
  providerName: string;
  providerEnabled: boolean;
  /** Provider identity behind the vendor icon marker; empty when the provider is unknown. */
  providerIcon: string;
  providerBaseUrl: string;
  /** Display label component: the prefix plays the model-name role. */
  model: string;
  score: number;
  scoreUpdatedAt: string;
  status: "ready" | "missing";
  tps: number | null;
  overallTps: number | null;
  selectedRequestCount: number;
  validTpsCount: number;
  firstSampleAt: number | null;
  lastSampleAt: number | null;
  unclassifiedCount: number;
  untrustedCount: number;
  variants: PerformanceRowVariant[];
}
export interface PerformancePoint extends PerformanceRow {
  status: "ready";
  score: number;
  tps: number;
  netTps?: number | null;
  metric?: "net" | "overall";
  color: string;
}
/** Hidden selection is retained for restoring filters but must not dim unrelated visible points. */
export function visiblePerformanceSelection(points: Pick<PerformancePoint, "key">[], hover: string[], pinned: string[]): string[] {
  const visible = new Set(points.map((point) => point.key));
  const visibleHover = hover.filter((key) => visible.has(key));
  return visibleHover.length ? visibleHover : pinned.filter((key) => visible.has(key));
}
const COLORS = ["#2563eb", "#0d9488", "#b45309", "#9333ea", "#dc2626", "#0891b2", "#db2777", "#4f46e5"];
export function performanceColor(providerId: string): string {
  let hash = 0;
  for (const code of providerId) hash = (hash * 31 + code.codePointAt(0)!) >>> 0;
  return COLORS[hash % COLORS.length];
}
/** One aggregated point per (prefix × provider); backend did the weighted merge. */
export function buildPerformanceRows(snapshot: PerformanceResponse, providers: Provider[]): PerformanceRow[] {
  const providerIndex = new Map(providers.map((provider) => [provider.id, provider]));
  const rows: PerformanceRow[] = [];
  for (const item of snapshot.models) {
    const provider = providerIndex.get(item.provider_id);
    const stats = item.mixed;
    rows.push({
      key: JSON.stringify([item.model_prefix, item.provider_id]), pointId: "",
      modelPrefix: item.model_prefix, model: item.model_prefix,
      providerId: item.provider_id, providerName: provider?.name ?? item.provider_id,
      providerEnabled: provider?.is_enabled ?? false,
      providerIcon: provider?.preset_key ?? provider?.vendor ?? "",
      providerBaseUrl: provider?.base_url ?? "",
      score: item.score, scoreUpdatedAt: item.score_updated_at,
      status: stats.average_tps === null || stats.valid_tps_count === 0 ? "missing" : "ready",
      tps: stats.average_tps, overallTps: stats.overall_tps ?? null,
      selectedRequestCount: stats.selected_request_count, validTpsCount: stats.valid_tps_count,
      firstSampleAt: stats.first_sample_at, lastSampleAt: stats.last_sample_at,
      unclassifiedCount: item.unclassified_count, untrustedCount: item.untrusted_count,
      variants: item.variants.map((variant) => ({ upstream_model: variant.upstream_model, stats: variant.mixed })),
    });
  }
  // Exact identities, independent of names, input order, filters, and chart zoom.
  return rows.sort((a, b) => a.key < b.key ? -1 : a.key > b.key ? 1 : 0)
    .map((row, index) => ({ ...row, pointId: `P${String(index + 1).padStart(2, "0")}` }));
}
export function filterPerformanceRows(rows: PerformanceRow[], search: string, providerId: string | null): PerformanceRow[] {
  const text = search.toLocaleLowerCase();
  return rows.filter((row) => (providerId === null || row.providerId === providerId)
    && `${row.providerName}\n${row.providerId}\n${row.modelPrefix}\n${row.variants.map((variant) => variant.upstream_model).join("\n")}`.toLocaleLowerCase().includes(text));
}
export function performancePoints(rows: PerformanceRow[], metric: "net" | "overall" = "net"): PerformancePoint[] {
  if (metric === "overall") {
    return rows.filter((row): row is PerformanceRow & { status: "ready"; overallTps: number } => (
      row.status === "ready" && row.overallTps !== null && Number.isFinite(row.overallTps) && row.overallTps > 0
    )).map((row) => ({
      ...row,
      tps: row.overallTps,
      netTps: row.tps,
      metric: "overall",
      color: performanceColor(row.providerId),
    }));
  }
  return rows.filter((row): row is PerformanceRow & { status: "ready"; tps: number } => (
    row.status === "ready" && row.tps !== null && Number.isFinite(row.tps) && row.tps > 0
  )).map((row) => ({
    ...row,
    netTps: row.tps,
    metric: "net",
    color: performanceColor(row.providerId),
  }));
}
export interface PerformanceEnvelopeNode {
  score: number;
  tps: number;
  memberKeys: string[];
}
export interface PerformanceEnvelope {
  nodes: PerformanceEnvelopeNode[];
  memberKeys: Set<string>;
}

/**
 * Upper-right convex envelope in score/TPS space, not the full Pareto frontier.
 * All visible valid points participate, including low-sample points. Preserve
 * collinear boundary members and all identities at an exactly coincident point.
 * Work on copies; neither display rounding nor the viewport changes membership.
 */
export function buildPerformanceEnvelope(points: PerformancePoint[]): PerformanceEnvelope {
  const positions = new Map<string, PerformanceEnvelopeNode>();
  for (const point of points) {
    if (!Number.isInteger(point.score) || point.score < 0 || point.score > 100
      || !Number.isFinite(point.tps) || point.tps <= 0 || point.status !== "ready") continue;
    const positionKey = JSON.stringify([point.score, point.tps]);
    const existing = positions.get(positionKey);
    if (existing) existing.memberKeys.push(point.key);
    else positions.set(positionKey, { score: point.score, tps: point.tps, memberKeys: [point.key] });
  }
  const sorted = [...positions.values()].sort((a, b) => b.score - a.score || b.tps - a.tps);
  for (const node of sorted) node.memberKeys.sort();

  // A weaker score at the same TPS (or slower TPS at the same score) is dominated.
  const candidates: PerformanceEnvelopeNode[] = [];
  let lastScore: number | undefined;
  let fastest = -Infinity;
  for (const node of sorted) {
    if (node.score === lastScore) continue;
    lastScore = node.score;
    if (node.tps <= fastest) continue;
    candidates.push(node);
    fastest = node.tps;
  }
  candidates.reverse();

  const nodes: PerformanceEnvelopeNode[] = [];
  for (const node of candidates) {
    while (nodes.length >= 2) {
      const a = nodes[nodes.length - 2], b = nodes[nodes.length - 1];
      // Normalize only the cross-product arithmetic to avoid overflow. This is
      // a positive affine scaling, never rounding the underlying TPS or scores.
      const scale = Math.max(a.tps, b.tps, node.tps);
      const left = (b.score - a.score) * ((node.tps - a.tps) / scale);
      const right = ((b.tps - a.tps) / scale) * (node.score - a.score);
      const tolerance = 8 * Number.EPSILON * (Math.abs(left) + Math.abs(right));
      // Positive turn = inward dent below the A–C supporting segment.
      if (left - right <= tolerance) break;
      nodes.pop();
    }
    nodes.push(node);
  }
  return { nodes, memberKeys: new Set(nodes.flatMap((node) => node.memberKeys)) };
}

export function performanceTpsMaximum(points: Pick<PerformancePoint, "tps">[]): number {
  const max = points.reduce((value, point) => Math.max(value, point.tps), 0);
  return max <= 100 ? 100 : (Math.floor(max / 50) + 1) * 50;
}
export interface PerformanceScoreDomain {
  min: number;
  max: number;
}

/**
 * Fit the horizontal score range to visible points, snapping outwards to ten-point
 * ticks. A single score retains at least a 10-point span; an empty plot retains the
 * full 0–100 reference scale. Scores themselves are never transformed.
 */
export function performanceScoreDomain(points: Pick<PerformancePoint, "score">[]): PerformanceScoreDomain {
  if (!points.length) return { min: 0, max: 100 };
  const minimum = points.reduce((value, point) => Math.min(value, point.score), 100);
  const maximum = points.reduce((value, point) => Math.max(value, point.score), 0);
  let min = Math.max(0, Math.floor(minimum / 10) * 10);
  let max = Math.min(100, Math.ceil(maximum / 10) * 10);
  if (max === min) {
    if (max < 100) max = Math.min(100, max + 10);
    else min = Math.max(0, min - 10);
  }
  return { min, max };
}
/** @deprecated Use performanceScoreDomain so lower and upper bounds move together. */
export function performanceScoreMaximum(points: Pick<PerformancePoint, "score">[]): number {
  return performanceScoreDomain(points).max;
}
export const PERFORMANCE_CHART = { width: 800, height: 500, left: 66, right: 766, top: 28, bottom: 440 };
/** Marker geometry, shared by the chart and by the room it reserves at the score extremes. */
export const PERFORMANCE_MARKER_SIZE = 24;
export const PERFORMANCE_COINCIDENT_MARKER_SIZE = 20;
/**
 * Horizontal room kept between the extreme visible score and the Y axis, so the
 * leftmost/rightmost marker sits inside the plot instead of straddling the axis:
 * half a marker plus air. Scores, ticks and the envelope all share this mapping.
 */
export const PERFORMANCE_SCORE_MARGIN = PERFORMANCE_MARKER_SIZE / 2 + 6;
export interface PerformanceXSpan {
  left: number;
  right: number;
}
/** The X positions the visible score range maps onto: the axis frame minus the margin. */
export function performanceXSpan(): PerformanceXSpan {
  return {
    left: PERFORMANCE_CHART.left + PERFORMANCE_SCORE_MARGIN,
    right: PERFORMANCE_CHART.right - PERFORMANCE_SCORE_MARGIN,
  };
}
/** Single source of truth for score → X, shared by points, ticks and their labels. */
export function scoreX(score: number, xDomain: PerformanceScoreDomain = { min: 0, max: 100 }): number {
  const { left, right } = performanceXSpan();
  return left + (right - left) * (score - xDomain.min) / (xDomain.max - xDomain.min);
}
export function pointCoordinates(
  point: Pick<PerformancePoint, "score" | "tps">,
  yMax: number,
  xDomain: PerformanceScoreDomain = { min: 0, max: 100 },
) {
  const { top, bottom } = PERFORMANCE_CHART;
  return {
    x: scoreX(point.score, xDomain),
    y: bottom - (bottom - top) * point.tps / yMax,
  };
}
export interface PerformanceGroup { key: string; points: PerformancePoint[]; x: number; y: number }
export function groupPerformancePoints(
  points: PerformancePoint[],
  yMax: number,
  xDomain: PerformanceScoreDomain = { min: 0, max: 100 },
): PerformanceGroup[] {
  const groups = new Map<string, PerformanceGroup>();
  for (const point of points) {
    const key = JSON.stringify([point.score, point.tps]);
    const group = groups.get(key);
    if (group) group.points.push(point);
    else groups.set(key, { key, points: [point], ...pointCoordinates(point, yMax, xDomain) });
  }
  return [...groups.values()];
}
/** Spatial index of true centers; 28 covers overlap between two 14px hit targets. */
export function buildPerformanceHitIndex(groups: PerformanceGroup[], radius = 28) {
  const cells = new Map<string, PerformanceGroup[]>();
  for (const group of groups) {
    const key = `${Math.floor(group.x / radius)},${Math.floor(group.y / radius)}`;
    cells.set(key, [...(cells.get(key) ?? []), group]);
  }
  return (x: number, y: number): PerformancePoint[] => {
    const found: PerformancePoint[] = [];
    const cx = Math.floor(x / radius), cy = Math.floor(y / radius);
    for (let dx = -1; dx <= 1; dx++) for (let dy = -1; dy <= 1; dy++) {
      for (const group of cells.get(`${cx + dx},${cy + dy}`) ?? []) {
        if (Math.hypot(group.x - x, group.y - y) <= radius) found.push(...group.points);
      }
    }
    return found.sort((a, b) => a.pointId.localeCompare(b.pointId, "en", { numeric: true }));
  };
}
export interface PerformanceLabel { x: number; y: number; width: number; height: number; lines: string[] }
/** The prefix is primary; the provider disambiguates every point. */
export function performanceModelLabel(point: PerformancePoint): string {
  return `${point.modelPrefix} · ${point.providerName}`;
}
const characterWidth = (char: string) => char.codePointAt(0)! > 127 ? 11 : 6.5;
function wrapLabel(text: string, maxWidth: number): string[] {
  const lines: string[] = []; let line = "", width = 0;
  for (const char of text) {
    const next = characterWidth(char);
    if (line && width + next > maxWidth) { lines.push(line); line = ""; width = 0; }
    line += char; width += next;
  }
  if (line) lines.push(line);
  return lines;
}
/**
 * Only groups sitting on the visible convex envelope get direct labels;
 * interior and dominated groups surface their names on hover/focus instead.
 * A boundary position keeps every coincident model in one labeled group.
 */
export function envelopeLabelGroups(groups: PerformanceGroup[], envelope: PerformanceEnvelope): PerformanceGroup[] {
  return groups.filter((group) => group.points.some((point) => envelope.memberKeys.has(point.key)));
}
/** Move labels only, never points. Every coincident point gets its own full wrapped text. */
export function layoutPerformanceLabels(groups: PerformanceGroup[]): Map<string, PerformanceLabel> {
  const placed: PerformanceLabel[] = [];
  const result = new Map<string, PerformanceLabel>();
  const { left, right, top, bottom } = PERFORMANCE_CHART;
  for (const group of groups) {
    const lines = group.points.flatMap((point) => wrapLabel(performanceModelLabel(point), 194));
    const width = Math.min(210, Math.max(70, ...lines.map((line) => [...line].reduce((n, char) => n + characterWidth(char), 0) + 16)));
    const height = lines.length * 15 + 10;
    const slots: { x: number; y: number }[] = [];
    for (let y = top; y + height <= bottom; y += 12) for (let x = left; x + width <= right; x += 12) slots.push({ x, y });
    slots.sort((a, b) => Math.hypot(a.x + width / 2 - group.x, a.y + height / 2 - group.y)
      - Math.hypot(b.x + width / 2 - group.x, b.y + height / 2 - group.y));
    for (const slot of slots) {
      const box = { ...slot, width, height, lines };
      if (placed.some((p) => box.x < p.x + p.width + 4 && box.x + width + 4 > p.x && box.y < p.y + p.height + 4 && box.y + height + 4 > p.y)) continue;
      if (groups.some((p) => p.x >= box.x - 10 && p.x <= box.x + width + 10 && p.y >= box.y - 10 && p.y <= box.y + height + 10)) continue;
      placed.push(box); result.set(group.key, box); break;
    }
  }
  return result;
}
