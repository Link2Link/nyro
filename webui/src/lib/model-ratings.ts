import type { Model, ModelRatingEntry, Provider } from "./types";

/**
 * Case-insensitive segment-boundary prefix match, mirrored in Rust
 * (db/model_rating_prefixes.rs): the model equals the prefix or continues
 * after a "-", comparing both sides lowercased. Separators and whitespace stay
 * exact — only case differences are ignored.
 */
export function modelMatchesPrefix(model: string, prefix: string): boolean {
  const lowerModel = model.toLowerCase();
  const lowerPrefix = prefix.toLowerCase();
  if (lowerModel.length <= lowerPrefix.length) return lowerModel === lowerPrefix;
  return lowerModel.startsWith(lowerPrefix) && lowerModel.charCodeAt(lowerPrefix.length) === 0x2d;
}

export interface RatingMatch {
  entry: ModelRatingEntry;
  model: string;
}

/** Longest matching prefix wins, comparing lengths on the lowercased form. */
export function longestRatingMatch(
  model: string,
  entries: ModelRatingEntry[],
): ModelRatingEntry | null {
  let best: ModelRatingEntry | null = null;
  let bestLength = -1;
  for (const entry of entries) {
    if (!modelMatchesPrefix(model, entry.model_prefix)) continue;
    const length = entry.model_prefix.toLowerCase().length;
    if (length > bestLength) {
      best = entry;
      bestLength = length;
    }
  }
  return best;
}

/** All entries matching a model, shortest prefix first (diagnostics only). */
export function matchingEntries(model: string, entries: ModelRatingEntry[]): ModelRatingEntry[] {
  return entries
    .filter((entry) => modelMatchesPrefix(model, entry.model_prefix))
    .sort((a, b) => a.model_prefix.length - b.model_prefix.length);
}

/** Every (provider, model) a prefix entry would cover, grouped by provider. */
export interface PrefixCoverage {
  provider: Provider;
  models: string[];
}

export function prefixCoverage(
  prefix: string,
  catalogs: { providerId: string; models: string[] }[],
  providers: Provider[],
): PrefixCoverage[] {
  const providerIndex = new Map(providers.map((provider) => [provider.id, provider]));
  const coverage: PrefixCoverage[] = [];
  for (const catalog of catalogs) {
    const models = catalog.models.filter((model) => modelMatchesPrefix(model, prefix));
    if (models.length === 0) continue;
    const provider = providerIndex.get(catalog.providerId);
    if (provider) coverage.push({ provider, models });
  }
  return coverage.sort((a, b) => a.provider.name.localeCompare(b.provider.name, "en", { sensitivity: "base" }));
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

export function isValidRatingPrefix(prefix: string): boolean {
  return prefix.trim() !== "" && !prefix.includes("\0") && new Blob([prefix]).size <= 1024;
}

/** Canonical storage form, mirroring the backend's lowercase canonicalization. */
export function canonicalModelPrefix(prefix: string): string {
  return prefix.toLowerCase();
}

export function isModelRatingEntry(value: unknown): value is ModelRatingEntry {
  if (!value || typeof value !== "object") return false;
  const entry = value as Record<string, unknown>;
  return typeof entry.model_prefix === "string"
    && isValidRatingPrefix(entry.model_prefix)
    && typeof entry.score === "number"
    && Number.isInteger(entry.score)
    && entry.score >= 0 && entry.score <= 100
    && typeof entry.updated_at === "string"
    && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/i.test(entry.updated_at)
    && Number.isFinite(Date.parse(entry.updated_at));
}

export function readModelRatings(value: unknown): ModelRatingEntry[] {
  if (!Array.isArray(value) || !value.every(isModelRatingEntry)) {
    throw new Error("Invalid model ratings response. The backend may not support model ratings.");
  }
  const keys = new Set(value.map((entry) => entry.model_prefix));
  if (keys.size !== value.length) throw new Error("Duplicate model ratings in backend response.");
  return value;
}

export type RatingLoadState = "loading" | "error" | "ready";
export type RatingDisplayState =
  | { status: "loading" | "error" | "unrated" }
  | { status: "rated"; entry: ModelRatingEntry };

/**
 * Resolve the display state for one concrete model: the longest matching entry
 * is the score; no match is unrated. Loading/error states never masquerade as
 * unrated.
 */
export function ratingDisplayState(
  loadState: RatingLoadState,
  model: string,
  entries: ModelRatingEntry[],
): RatingDisplayState {
  if (loadState !== "ready") return { status: loadState };
  const matched = longestRatingMatch(model, entries);
  return matched ? { status: "rated", entry: matched } : { status: "unrated" };
}

export interface ModelCatalogSnapshot {
  providerId: string;
  status: "success" | "loading" | "unknown";
  models: string[];
}

export interface UnmatchedModelRow {
  key: string;
  provider: Provider;
  model: string;
}

/** Concrete models known from catalogs and route targets that no entry covers. */
export function buildUnmatchedModels(
  providers: Provider[],
  routes: Model[],
  catalogs: ModelCatalogSnapshot[],
  entries: ModelRatingEntry[],
): UnmatchedModelRow[] {
  const providerIndex = new Map(providers.map((provider) => [provider.id, provider]));
  const seen = new Set<string>();
  const rows: UnmatchedModelRow[] = [];
  function add(providerId: string, model: string) {
    const provider = providerIndex.get(providerId);
    if (!provider || provider.is_enabled === false) return;
    const key = JSON.stringify([providerId, model]);
    if (seen.has(key)) return;
    seen.add(key);
    if (longestRatingMatch(model, entries)) return;
    rows.push({ key, provider, model });
  }
  for (const catalog of catalogs) {
    if (catalog.status === "success") {
      for (const model of catalog.models) add(catalog.providerId, model);
    }
  }
  for (const route of routes) {
    const targets = route.targets?.length
      ? route.targets
      : [{ provider_id: route.target_provider, model: route.target_model }];
    for (const target of targets) add(target.provider_id, target.model);
  }
  return rows.sort((a, b) => (
    a.model.localeCompare(b.model, "en", { sensitivity: "base" })
    || a.provider.name.localeCompare(b.provider.name, "en", { sensitivity: "base" })
  ));
}

export type RatingFilter = "all" | "matched" | "unmatched";
export type RatingSort = "score-desc" | "score-asc" | "name" | "updated";
export interface ModelRatingFilters {
  search: string;
  rating: RatingFilter;
  min: number | null;
  max: number | null;
  sort: RatingSort;
  ratingsReady: boolean;
  coverageReady: boolean;
}

function normalizeSearch(value: string): string {
  return value.toLocaleLowerCase().replace(/[\s._\p{Pd}/:]+/gu, "");
}

export interface ModelRatingRow {
  key: string;
  entry: ModelRatingEntry;
  /** Known concrete (provider, model) hits; empty when coverage is unknown. */
  coverage: PrefixCoverage[];
  modelCount: number;
  providerCount: number;
}

/** Rows for the prefix-entry table, with live coverage from known catalogs. */
export function buildModelRatingRows(
  entries: ModelRatingEntry[],
  catalogs: ModelCatalogSnapshot[],
  providers: Provider[],
): ModelRatingRow[] {
  return entries.map((entry) => {
    const coverage = prefixCoverage(entry.model_prefix, catalogs, providers);
    return {
      key: entry.model_prefix,
      entry,
      coverage,
      modelCount: coverage.reduce((total, group) => total + group.models.length, 0),
      providerCount: coverage.length,
    };
  });
}

/** All filtering/sorting is global. Callers paginate only the returned rows. */
export function filterAndSortModelRatingRows(
  rows: ModelRatingRow[],
  filters: ModelRatingFilters,
): ModelRatingRow[] {
  const search = normalizeSearch(filters.search);
  const filtered = rows.filter((row) => {
    if (search && ![row.entry.model_prefix]
      .some((value) => normalizeSearch(value).includes(search))) return false;
    // A failed score request must not turn unknown values into unmatched or zero.
    if (!filters.ratingsReady) return true;
    // Unknown coverage disables match-state predicates instead of excluding rows.
    if (filters.rating === "unmatched" && filters.coverageReady && row.modelCount > 0) return false;
    if (filters.rating === "matched" && filters.coverageReady && row.modelCount === 0) return false;
    if (filters.min !== null && row.entry.score < filters.min) return false;
    if (filters.max !== null && row.entry.score > filters.max) return false;
    return true;
  });
  return filtered.sort((a, b) => {
    if (filters.ratingsReady && (filters.sort === "score-desc" || filters.sort === "score-asc" || filters.sort === "updated")) {
      const diff = filters.sort === "updated"
        ? (Date.parse(b.entry.updated_at) || 0) - (Date.parse(a.entry.updated_at) || 0)
        : filters.sort === "score-asc" ? a.entry.score - b.entry.score : b.entry.score - a.entry.score;
      if (diff) return diff;
    }
    const nameDiff = a.entry.model_prefix.localeCompare(b.entry.model_prefix, "en", { sensitivity: "base" });
    return nameDiff || (a.entry.model_prefix < b.entry.model_prefix ? -1 : 1);
  });
}
