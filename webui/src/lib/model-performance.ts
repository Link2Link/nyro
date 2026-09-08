import { isProviderModelRatingProfile, providerModelKey } from "./model-ratings";
import { EFFORT_TIERS, type EffortTier, type Provider, type ProviderModelRatingProfile } from "./types";

export interface PerformanceStats {
  selected_request_count: number;
  valid_tps_count: number;
  average_tps: number | null;
  first_sample_at: number | null;
  last_sample_at: number | null;
}
export interface ModelPerformance {
  profile: ProviderModelRatingProfile;
  mixed: PerformanceStats;
  tiers: Record<EffortTier, PerformanceStats>;
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
    if (!model || !isProviderModelRatingProfile(model.profile)
      || !["ready", "error"].includes(model.status) || !isStats(model.mixed)
      || !model.tiers || !EFFORT_TIERS.every((tier) => isStats(model.tiers[tier]))
      || !count(model.unclassified_count) || !count(model.untrusted_count)
      || (model.error !== undefined && typeof model.error !== "string")) return fail();
    const key = providerModelKey(model.profile.provider_id, model.profile.upstream_model);
    if (keys.has(key)) return fail();
    keys.add(key);
  }
  return value as PerformanceResponse;
}
export type PerformanceTier = "mixed" | EffortTier;
export interface PerformanceRow {
  key: string;
  pointId: string;
  providerId: string;
  providerName: string;
  providerEnabled: boolean;
  model: string;
  tier: PerformanceTier;
  score: number | null;
  source: "override" | "common" | "unrated";
  scoreUpdatedAt: string | null;
  status: "ready" | "missing" | "unrated" | "error";
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
/** Server mode and effective scores are authoritative. Never infer fallback/ranking. */
export function buildPerformanceRows(snapshot: PerformanceResponse, providers: Provider[]): PerformanceRow[] {
  const providerIndex = new Map(providers.map((provider) => [provider.id, provider]));
  const rows: PerformanceRow[] = [];
  for (const model of snapshot.models) {
    const profile = model.profile;
    const provider = providerIndex.get(profile.provider_id);
    const tiers: PerformanceTier[] = profile.display_mode === "common" ? ["mixed"] : [...EFFORT_TIERS];
    for (const tier of tiers) {
      const effective = tier === "mixed" ? null : profile.effective[tier];
      const score = tier === "mixed" ? profile.common?.score ?? null : effective!.score;
      const source = tier === "mixed" ? (profile.common ? "common" : "unrated") : effective!.source;
      const stats = tier === "mixed" ? model.mixed : model.tiers[tier];
      rows.push({
        key: JSON.stringify([profile.provider_id, profile.upstream_model, tier]), pointId: "",
        providerId: profile.provider_id, providerName: provider?.name ?? profile.provider_id,
        providerEnabled: provider?.is_enabled ?? false, model: profile.upstream_model, tier,
        score, source, scoreUpdatedAt: tier === "mixed" ? profile.common?.updated_at ?? null : effective!.score_updated_at,
        status: model.status === "error" ? "error" : score === null ? "unrated"
          : stats.average_tps === null || stats.valid_tps_count === 0 ? "missing" : "ready",
        tps: stats.average_tps, selectedRequestCount: stats.selected_request_count, validTpsCount: stats.valid_tps_count,
        firstSampleAt: stats.first_sample_at, lastSampleAt: stats.last_sample_at,
        unclassifiedCount: model.unclassified_count, untrustedCount: model.untrusted_count, error: model.error,
      });
    }
  }
  // Exact identities, independent of names, input order, filters, and chart zoom.
  return rows.sort((a, b) => a.key < b.key ? -1 : a.key > b.key ? 1 : 0)
    .map((row, index) => ({ ...row, pointId: `P${String(index + 1).padStart(2, "0")}` }));
}
export function filterPerformanceRows(rows: PerformanceRow[], search: string, providerId: string | null, tier: PerformanceTier | null): PerformanceRow[] {
  const text = search.toLocaleLowerCase();
  return rows.filter((row) => (providerId === null || row.providerId === providerId) && (tier === null || row.tier === tier)
    && `${row.pointId}\n${row.providerName}\n${row.providerId}\n${row.model}`.toLocaleLowerCase().includes(text));
}
export function performancePoints(rows: PerformanceRow[]): PerformancePoint[] {
  return rows.filter((row): row is PerformanceRow & { status: "ready"; score: number; tps: number } => (
    row.status === "ready" && row.tps !== null && Number.isFinite(row.tps) && row.tps > 0
    && row.score !== null && Number.isInteger(row.score) && row.score >= 0 && row.score <= 100
  )).map((row) => ({ ...row, color: performanceColor(row.providerId) }));
}
export function performanceTpsMaximum(points: Pick<PerformancePoint, "tps">[]): number {
  const max = points.reduce((value, point) => Math.max(value, point.tps), 0);
  return max <= 200 ? 200 : (Math.floor(max / 50) + 1) * 50;
}
export function performanceTierLabel(tier: PerformanceTier, isZh: boolean): string {
  if (tier === "mixed") return isZh ? "混合" : "Mixed";
  if (tier === "low") return isZh ? "low（含 minimal）" : "low (includes minimal)";
  return tier;
}
export const PERFORMANCE_CHART = { width: 800, height: 500, left: 66, right: 766, top: 28, bottom: 440 };
export function pointCoordinates(point: Pick<PerformancePoint, "score" | "tps">, yMax: number) {
  const { left, right, top, bottom } = PERFORMANCE_CHART;
  return { x: left + (right - left) * point.score / 100, y: bottom - (bottom - top) * point.tps / yMax };
}
export interface PerformanceGroup { key: string; points: PerformancePoint[]; x: number; y: number }
export function groupPerformancePoints(points: PerformancePoint[], yMax: number): PerformanceGroup[] {
  const groups = new Map<string, PerformanceGroup>();
  for (const point of points) {
    const key = JSON.stringify([point.score, point.tps]);
    const group = groups.get(key);
    if (group) group.points.push(point);
    else groups.set(key, { key, points: [point], ...pointCoordinates(point, yMax) });
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
export interface PerformanceLabel { x: number; y: number; width: number; height: number }
/** Labels only move; if no collision-free slot exists, the numbered index remains available. */
export function layoutPerformanceLabels(groups: PerformanceGroup[]): Map<string, PerformanceLabel> {
  const placed: PerformanceLabel[] = [];
  const result = new Map<string, PerformanceLabel>();
  const { left, right, top, bottom } = PERFORMANCE_CHART;
  for (const group of groups) {
    const width = 12 + (group.points[0].pointId.length + (group.points.length > 1 ? String(group.points.length).length + 2 : 0)) * 7;
    search: for (let lane = 0; lane < 12; lane++) for (const side of [1, -1]) for (const direction of [-1, 1]) {
      const box = { x: Math.max(left, Math.min(right - width, side > 0 ? group.x + 12 : group.x - width - 12)),
        y: Math.max(top, Math.min(bottom - 18, group.y + direction * (12 + lane * 22) - 9)), width, height: 18 };
      if (placed.some((p) => box.x < p.x + p.width + 4 && box.x + width + 4 > p.x && box.y < p.y + p.height + 3 && box.y + 21 > p.y)) continue;
      if (groups.some((p) => p.x >= box.x - 8 && p.x <= box.x + width + 8 && p.y >= box.y - 8 && p.y <= box.y + 26)) continue;
      placed.push(box); result.set(group.key, box); break search;
    }
  }
  return result;
}
