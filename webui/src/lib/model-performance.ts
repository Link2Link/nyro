import { isProviderModelRating, providerModelKey } from "./model-ratings";
import type { Provider, ProviderModelRating } from "./types";

export interface PerformanceStats {
  selected_request_count: number;
  valid_tps_count: number;
  average_tps: number | null;
  first_sample_at: number | null;
  last_sample_at: number | null;
}
export interface ModelPerformance {
  rating: ProviderModelRating;
  mixed: PerformanceStats;
  unclassified_count: number;
  untrusted_count: number;
  status: "ready" | "error";
  error?: string;
}
export interface PerformanceResponse { as_of: number; window_start: number; models: ModelPerformance[] }
const count = (value: unknown) => typeof value === "number" && Number.isInteger(value) && value >= 0;
const finite = (value: unknown) => typeof value === "number" && Number.isFinite(value);
function isStats(value: unknown): value is PerformanceStats {
  if (!value || typeof value !== "object") return false;
  const v = value as Record<string, unknown>;
  if (!count(v.selected_request_count) || Number(v.selected_request_count) > 10
    || !count(v.valid_tps_count) || Number(v.valid_tps_count) > Number(v.selected_request_count)) return false;
  if (v.valid_tps_count === 0) return v.average_tps === null && v.first_sample_at === null && v.last_sample_at === null;
  return finite(v.average_tps) && Number(v.average_tps) > 0
    && finite(v.first_sample_at) && Number(v.first_sample_at) >= 0
    && finite(v.last_sample_at) && Number(v.last_sample_at) >= Number(v.first_sample_at);
}
/** Validate the batch as a contract, not as invented zero-valued statistics. */
export function readPerformanceResponse(value: unknown): PerformanceResponse {
  const fail = () => { throw new Error("Invalid model performance response. Please check backend compatibility."); };
  if (!value || typeof value !== "object") return fail();
  const v = value as Record<string, unknown>;
  if (!finite(v.as_of) || Number(v.as_of) < 0 || !finite(v.window_start) || Number(v.window_start) < 0
    || Number(v.window_start) > Number(v.as_of) || !Array.isArray(v.models)) return fail();
  const keys = new Set<string>();
  for (const model of v.models) {
    if (!model || !isProviderModelRating(model.rating)
      || !["ready", "error"].includes(model.status) || !isStats(model.mixed)
      || !count(model.unclassified_count) || !count(model.untrusted_count)
      || (model.error !== undefined && typeof model.error !== "string")) return fail();
    const key = providerModelKey(model.rating.provider_id, model.rating.upstream_model);
    if (keys.has(key)) return fail();
    keys.add(key);
  }
  return value as PerformanceResponse;
}
export interface PerformanceRow {
  key: string;
  pointId: string;
  providerId: string;
  providerName: string;
  providerEnabled: boolean;
  model: string;
  score: number;
  scoreUpdatedAt: string | null;
  status: "ready" | "missing" | "error";
  tps: number | null;
  selectedRequestCount: number;
  validTpsCount: number;
  firstSampleAt: number | null;
  lastSampleAt: number | null;
  unclassifiedCount: number;
  untrustedCount: number;
  error?: string;
}
export interface PerformancePoint extends PerformanceRow { status: "ready"; score: number; tps: number; color: string }
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
/** One comprehensive rating and mixed statistic per exact provider/model pair. */
export function buildPerformanceRows(snapshot: PerformanceResponse, providers: Provider[]): PerformanceRow[] {
  const providerIndex = new Map(providers.map((provider) => [provider.id, provider]));
  const rows: PerformanceRow[] = [];
  for (const model of snapshot.models) {
    const rating = model.rating;
    const provider = providerIndex.get(rating.provider_id);
    const stats = model.mixed;
    rows.push({
      key: providerModelKey(rating.provider_id, rating.upstream_model), pointId: "",
      providerId: rating.provider_id, providerName: provider?.name ?? rating.provider_id,
      providerEnabled: provider?.is_enabled ?? false, model: rating.upstream_model,
      score: rating.score, scoreUpdatedAt: rating.updated_at,
      status: model.status === "error" ? "error"
        : stats.average_tps === null || stats.valid_tps_count === 0 ? "missing" : "ready",
      tps: stats.average_tps, selectedRequestCount: stats.selected_request_count, validTpsCount: stats.valid_tps_count,
      firstSampleAt: stats.first_sample_at, lastSampleAt: stats.last_sample_at,
      unclassifiedCount: model.unclassified_count, untrustedCount: model.untrusted_count, error: model.error,
    });
  }
  // Exact identities, independent of names, input order, filters, and chart zoom.
  return rows.sort((a, b) => a.key < b.key ? -1 : a.key > b.key ? 1 : 0)
    .map((row, index) => ({ ...row, pointId: `P${String(index + 1).padStart(2, "0")}` }));
}
export function filterPerformanceRows(rows: PerformanceRow[], search: string, providerId: string | null): PerformanceRow[] {
  const text = search.toLocaleLowerCase();
  return rows.filter((row) => (providerId === null || row.providerId === providerId)
    && `${row.providerName}\n${row.providerId}\n${row.model}`.toLocaleLowerCase().includes(text));
}
export function performancePoints(rows: PerformanceRow[]): PerformancePoint[] {
  return rows.filter((row): row is PerformanceRow & { status: "ready"; score: number; tps: number } => (
    row.status === "ready" && row.tps !== null && Number.isFinite(row.tps) && row.tps > 0
    && row.score !== null && Number.isInteger(row.score) && row.score >= 0 && row.score <= 100
  )).map((row) => ({ ...row, color: performanceColor(row.providerId) }));
}
export function performanceTpsMaximum(points: Pick<PerformancePoint, "tps">[]): number {
  const max = points.reduce((value, point) => Math.max(value, point.tps), 0);
  return max <= 100 ? 100 : (Math.floor(max / 50) + 1) * 50;
}
/** Keep a zero baseline and round the visible maximum to a readable score boundary. */
export function performanceScoreMaximum(points: Pick<PerformancePoint, "score">[]): number {
  if (!points.length) return 100;
  const max = points.reduce((value, point) => Math.max(value, point.score), 0);
  return Math.min(100, Math.max(10, Math.ceil(max / 10) * 10));
}
export const PERFORMANCE_CHART = { width: 800, height: 500, left: 66, right: 766, top: 28, bottom: 440 };
export function pointCoordinates(point: Pick<PerformancePoint, "score" | "tps">, yMax: number, xMax = 100) {
  const { left, right, top, bottom } = PERFORMANCE_CHART;
  return { x: left + (right - left) * point.score / xMax, y: bottom - (bottom - top) * point.tps / yMax };
}
export interface PerformanceGroup { key: string; points: PerformancePoint[]; x: number; y: number }
export function groupPerformancePoints(points: PerformancePoint[], yMax: number, xMax = 100): PerformanceGroup[] {
  const groups = new Map<string, PerformanceGroup>();
  for (const point of points) {
    const key = JSON.stringify([point.score, point.tps]);
    const group = groups.get(key);
    if (group) group.points.push(point);
    else groups.set(key, { key, points: [point], ...pointCoordinates(point, yMax, xMax) });
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
/** Model names are primary; disambiguate only when suppliers share the same model. */
export function performanceModelLabel(point: PerformancePoint, points: PerformancePoint[]): string {
  const ambiguous = points.some((other) => other.providerId !== point.providerId
    && other.providerName === point.providerName && other.model === point.model);
  const sameModel = points.some((other) => other.providerId !== point.providerId && other.model === point.model);
  return `${point.model}${sameModel ? ` · ${point.providerName}${ambiguous ? ` (${point.providerId.slice(0, 8)})` : ""}` : ""}`;
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
/** Move labels only, never points. Every coincident model gets its own full wrapped text. */
export function layoutPerformanceLabels(groups: PerformanceGroup[]): Map<string, PerformanceLabel> {
  const placed: PerformanceLabel[] = [];
  const result = new Map<string, PerformanceLabel>();
  const points = groups.flatMap((group) => group.points);
  const { left, right, top, bottom } = PERFORMANCE_CHART;
  for (const group of groups) {
    const lines = group.points.flatMap((point) => wrapLabel(performanceModelLabel(point, points), 194));
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
