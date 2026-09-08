import type { Model, Provider, ProviderModelRating } from "./types";
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

export function isProviderModelRating(value: unknown): value is ProviderModelRating {
  if (!value || typeof value !== "object") return false;
  const rating = value as Record<string, unknown>;
  return typeof rating.provider_id === "string"
    && typeof rating.upstream_model === "string"
    && typeof rating.score === "number"
    && Number.isInteger(rating.score)
    && rating.score >= 0 && rating.score <= 100
    && typeof rating.updated_at === "string"
    && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/i.test(rating.updated_at)
    && Number.isFinite(Date.parse(rating.updated_at));
}

export function readProviderModelRatings(value: unknown): ProviderModelRating[] {
  if (!Array.isArray(value) || !value.every(isProviderModelRating)) {
    throw new Error("Invalid model ratings response. The backend may not support model ratings.");
  }
  const keys = new Set(value.map((rating) => providerModelKey(rating.provider_id, rating.upstream_model)));
  if (keys.size !== value.length) throw new Error("Duplicate model ratings in backend response.");
  return value;
}

export type RatingLoadState = "loading" | "error" | "ready";
export type RatingDisplayState =
  | { status: "loading" | "error" | "unrated" }
  | { status: "rated"; rating: ProviderModelRating };

export function ratingDisplayState(
  loadState: RatingLoadState,
  rating?: ProviderModelRating | null,
  providerKnown = true,
): RatingDisplayState {
  if (loadState !== "ready") return { status: loadState };
  if (rating) return { status: "rated", rating };
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
  rating: ProviderModelRating | null;
  catalogStatus: "listed" | "missing" | "unknown";
}

/** Union raw catalogs, route references (including disabled routes), and saved scores. */
export function buildModelRatingRows(
  providers: Provider[],
  routes: Model[],
  ratings: ProviderModelRating[],
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
    if (filters.rating === "rated" && !row.rating) return false;
    if (filters.rating === "unrated" && (row.rating || !row.provider)) return false;
    if (filters.min !== null && (!row.rating || row.rating.score < filters.min)) return false;
    if (filters.max !== null && (!row.rating || row.rating.score > filters.max)) return false;
    return true;
  });
  return filtered.sort((a, b) => {
    if (filters.ratingsReady && (filters.sort === "score-desc" || filters.sort === "score-asc" || filters.sort === "updated")) {
      // Absence is never zero: unrated rows stay last in both score directions.
      if (!!a.rating !== !!b.rating) return a.rating ? -1 : 1;
      if (a.rating && b.rating) {
        const diff = filters.sort === "updated"
          ? (parseBackendTime(b.rating.updated_at)?.getTime() ?? 0) - (parseBackendTime(a.rating.updated_at)?.getTime() ?? 0)
          : filters.sort === "score-asc" ? a.rating.score - b.rating.score : b.rating.score - a.rating.score;
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
