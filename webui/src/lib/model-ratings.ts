import { EFFORT_TIERS, type EffortTier, type EffectiveModelRating, type Model, type Provider, type ProviderModelRatingProfile, type RatingValue, type SetProviderModelRatingProfile } from "./types";
import { parseBackendTime } from "./format";

/** Do not normalize either part: whitespace, case, slashes, and separators are identity. */
export function providerModelKey(providerId: string, model: string): string {
  return JSON.stringify([providerId, model]);
}

export function uniqueModelIdentifiers(values: string[]): string[] {
  if (!Array.isArray(values) || !values.every((value) => typeof value === "string")) {
    throw new Error("Invalid model catalog response.");
  }
  return [...new Set(values)].sort((a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }));
}

/** Validate the entire draft before converting it; never trim, round, or clamp. */
export function parseRatingScore(draft: string): number | null {
  if (draft === "" || /[^0-9]/.test(draft)) return null;
  const score = Number(draft);
  return Number.isInteger(score) && score >= 0 && score <= 100 ? score : null;
}

function isScore(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= 100;
}

function isTimestamp(value: unknown): value is string {
  return typeof value === "string"
    && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/i.test(value)
    && Number.isFinite(Date.parse(value));
}

function isRatingValue(value: unknown): value is RatingValue {
  if (!value || typeof value !== "object") return false;
  const rating = value as Record<string, unknown>;
  return isScore(rating.score) && isTimestamp(rating.updated_at);
}

export function isProviderModelRatingProfile(value: unknown): value is ProviderModelRatingProfile {
  if (!value || typeof value !== "object") return false;
  const profile = value as ProviderModelRatingProfile;
  if (typeof profile.provider_id !== "string" || typeof profile.upstream_model !== "string"
    || (profile.common !== null && !isRatingValue(profile.common))
    || !profile.overrides || typeof profile.overrides !== "object"
    || !profile.effective || typeof profile.effective !== "object"
    || !["common", "per_effort"].includes(profile.display_mode)) return false;
  for (const tier of EFFORT_TIERS) {
    const override = profile.overrides[tier];
    if (override !== null && !isRatingValue(override)) return false;
    const effective = profile.effective[tier];
    if (!effective || typeof effective !== "object") return false;
    if (effective.status === "rated") {
      if (!isScore(effective.score) || !["override", "common"].includes(effective.source)
        || !isTimestamp(effective.score_updated_at)) return false;
    } else if (effective.status !== "unrated" || effective.score !== null
      || effective.source !== "unrated" || effective.score_updated_at !== null) return false;
  }
  return true;
}

export function readProviderModelRatingProfiles(value: unknown): ProviderModelRatingProfile[] {
  if (!Array.isArray(value) || !value.every(isProviderModelRatingProfile)) {
    throw new Error("Invalid model rating profiles response. Please check backend support and retry.");
  }
  const keys = new Set(value.map((rating) => providerModelKey(rating.provider_id, rating.upstream_model)));
  if (keys.size !== value.length) throw new Error("Duplicate model rating profiles in backend response.");
  return value;
}

export function countRatingOverrides(profile: ProviderModelRatingProfile): number {
  return EFFORT_TIERS.filter((tier) => profile.overrides[tier] !== null).length;
}

export function hasModelRating(profile?: ProviderModelRatingProfile | null): boolean {
  return Boolean(profile && (profile.common !== null || countRatingOverrides(profile) > 0));
}

export type RatingDimension = "common" | EffortTier;

export function ratingDimensionLabel(dimension: RatingDimension, isZh = false): string {
  if (dimension === "common") return isZh ? "通用" : "Common";
  if (dimension === "low") return isZh ? "低（含 minimal）" : "Low (includes minimal)";
  const labels = { medium: ["Medium", "中"], high: ["High", "高"], xhigh: ["Extra high", "超高"], max: ["Max", "最高"] };
  return labels[dimension][isZh ? 1 : 0];
}

/** Effort scores and sources are authoritative backend effective values, not inferred in views. */
export function ratingForDimension(profile: ProviderModelRatingProfile | null | undefined, dimension: RatingDimension): EffectiveModelRating {
  if (!profile) return { status: "unrated", score: null, source: "unrated", score_updated_at: null };
  if (dimension !== "common") return profile.effective[dimension];
  return profile.common
    ? { status: "rated", score: profile.common.score, source: "common", score_updated_at: profile.common.updated_at }
    : { status: "unrated", score: null, source: "unrated", score_updated_at: null };
}

export function emptyRatingProfileInput(): SetProviderModelRatingProfile {
  return { common: null, overrides: { low: null, medium: null, high: null, xhigh: null, max: null } };
}

export interface RatingProfileDraft {
  commonEnabled: boolean;
  common: string;
  overrides: Record<EffortTier, { enabled: boolean; score: string }>;
}

export function ratingProfileDraft(profile: ProviderModelRatingProfile | null): RatingProfileDraft {
  return {
    commonEnabled: profile?.common != null,
    common: profile?.common ? String(profile.common.score) : "",
    overrides: Object.fromEntries(EFFORT_TIERS.map((tier) => [tier, {
      enabled: profile?.overrides[tier] != null,
      score: profile?.overrides[tier] ? String(profile.overrides[tier].score) : "",
    }])) as RatingProfileDraft["overrides"],
  };
}

/** Explicit equal overrides are preserved; unsetting common never alters any override. */
export function parseRatingProfileDraft(draft: RatingProfileDraft): SetProviderModelRatingProfile | null {
  const input = emptyRatingProfileInput();
  if (draft.commonEnabled) {
    input.common = parseRatingScore(draft.common);
    if (input.common === null) return null;
  }
  for (const tier of EFFORT_TIERS) {
    if (!draft.overrides[tier].enabled) continue;
    input.overrides[tier] = parseRatingScore(draft.overrides[tier].score);
    if (input.overrides[tier] === null) return null;
  }
  return input;
}

export type RatingLoadState = "loading" | "error" | "ready";
export type RatingDisplayState =
  | { status: "loading" | "error" | "unrated" }
  | { status: "rated"; rating: ProviderModelRatingProfile };

export function ratingDisplayState(
  loadState: RatingLoadState,
  rating?: ProviderModelRatingProfile | null,
  providerKnown = true,
): RatingDisplayState {
  if (loadState !== "ready") return { status: loadState };
  if (rating && hasModelRating(rating)) return { status: "rated", rating };
  return { status: providerKnown ? "unrated" : "error" };
}

export interface ModelCatalogSnapshot {
  providerId: string;
  status: "success" | "loading" | "unknown";
  models: string[];
}

export interface ModelRatingRow {
  key: string;
  providerId: string;
  provider?: Provider;
  model: string;
  rating: ProviderModelRatingProfile | null;
  catalogStatus: "listed" | "missing" | "unknown";
}

/** Union raw catalogs, route references (including disabled routes), and saved scores. */
export function buildModelRatingRows(
  providers: Provider[],
  routes: Model[],
  ratings: ProviderModelRatingProfile[],
  catalogs: ModelCatalogSnapshot[],
): ModelRatingRow[] {
  const providerIndex = new Map(providers.map((provider) => [provider.id, provider]));
  const catalogIndex = new Map(catalogs.map((catalog) => [catalog.providerId, catalog]));
  const catalogModels = new Map(catalogs.map((catalog) => [catalog.providerId, new Set(catalog.models)]));
  const ratingIndex = new Map(ratings.map((rating) => [providerModelKey(rating.provider_id, rating.upstream_model), rating]));
  const rows = new Map<string, ModelRatingRow>();
  function add(providerId: string, model: string) {
    const key = providerModelKey(providerId, model);
    if (rows.has(key)) return;
    const provider = providerIndex.get(providerId);
    const catalog = catalogIndex.get(providerId);
    const catalogKnown = provider?.is_enabled && catalog?.status === "success";
    rows.set(key, {
      key, providerId, provider, model,
      rating: ratingIndex.get(key) ?? null,
      catalogStatus: !catalogKnown ? "unknown" : catalogModels.get(providerId)?.has(model) ? "listed" : "missing",
    });
  }
  for (const catalog of catalogs) {
    if (catalog.status === "success" && providerIndex.get(catalog.providerId)?.is_enabled) {
      for (const model of catalog.models) add(catalog.providerId, model);
    }
  }
  for (const route of routes) {
    const targets = route.targets?.length
      ? route.targets
      : [{ provider_id: route.target_provider, model: route.target_model }];
    for (const target of targets) add(target.provider_id, target.model);
  }
  for (const rating of ratings) add(rating.provider_id, rating.upstream_model);
  return [...rows.values()];
}

export type RatingFilter = "all" | "rated" | "unrated";
export type RatingSort = "score-desc" | "score-asc" | "name" | "updated";
export interface ModelRatingFilters {
  search: string;
  providerId: string | null;
  rating: RatingFilter;
  min: number | null;
  max: number | null;
  sort: RatingSort;
  ratingsReady: boolean;
  dimension?: RatingDimension;
}

function normalizeSearch(value: string): string {
  return value.toLocaleLowerCase().replace(/[\s._\p{Pd}/:]+/gu, "");
}

function compareExact(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

function compareName(a: string, b: string): number {
  return a.localeCompare(b, "en", { sensitivity: "base" });
}

function compareRowIdentity(a: ModelRatingRow, b: ModelRatingRow): number {
  return compareName(a.provider?.name ?? a.providerId, b.provider?.name ?? b.providerId)
    || compareName(a.model, b.model)
    || compareExact(a.providerId, b.providerId)
    || compareExact(a.model, b.model);
}

/** All filtering/sorting is global. Callers paginate only the returned rows. */
export function filterAndSortModelRatingRows(rows: ModelRatingRow[], filters: ModelRatingFilters): ModelRatingRow[] {
  const search = normalizeSearch(filters.search);
  const filtered = rows.filter((row) => {
    if (filters.providerId !== null && row.providerId !== filters.providerId) return false;
    if (search && ![row.provider?.name ?? "", row.providerId, row.model]
      .some((value) => normalizeSearch(value).includes(search))) return false;
    // A failed score request must not turn unknown values into unrated/zero or fake empty results.
    if (!filters.ratingsReady) return true;
    const value = ratingForDimension(row.rating, filters.dimension ?? "common");
    if (filters.rating === "rated" && value.status !== "rated") return false;
    if (filters.rating === "unrated" && (value.status === "rated" || !row.provider)) return false;
    if (filters.min !== null && (value.score === null || value.score < filters.min)) return false;
    if (filters.max !== null && (value.score === null || value.score > filters.max)) return false;
    return true;
  });
  return filtered.sort((a, b) => {
    if (filters.ratingsReady && (filters.sort === "score-desc" || filters.sort === "score-asc" || filters.sort === "updated")) {
      // Absence is never zero: unrated rows stay last in both score directions.
      const av = ratingForDimension(a.rating, filters.dimension ?? "common");
      const bv = ratingForDimension(b.rating, filters.dimension ?? "common");
      if (av.status !== bv.status) return av.status === "rated" ? -1 : 1;
      if (av.status === "rated" && bv.status === "rated") {
        const diff = filters.sort === "updated"
          ? (parseBackendTime(bv.score_updated_at)?.getTime() ?? 0) - (parseBackendTime(av.score_updated_at)?.getTime() ?? 0)
          : filters.sort === "score-asc" ? av.score - bv.score : bv.score - av.score;
        if (diff) return diff;
      }
    }
    if (filters.sort === "name") {
      const diff = compareName(a.model, b.model);
      if (diff) return diff;
    }
    return compareRowIdentity(a, b);
  });
}
