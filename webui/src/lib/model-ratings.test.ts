import { deepEqual, equal, notEqual, ok, throws } from "node:assert/strict";
import { test } from "node:test";
import {
  buildModelRatingRows, buildUnmatchedModels, canonicalModelPrefix, filterAndSortModelRatingRows, isValidRatingPrefix,
  longestRatingMatch, matchingEntries, modelMatchesPrefix, parseRatingScore, prefixCoverage,
  ratingDisplayState, readModelRatings, uniqueModelIdentifiers,
  type ModelCatalogSnapshot, type ModelRatingFilters,
} from "./model-ratings";
import type { Model, ModelRatingEntry, Provider } from "./types";

// No test framework needed: compile this file with tsc --module commonjs into a temporary
// directory outside webui, then run node --test <temp>/model-ratings.test.js.
// Boundary cases mirror crates/nyro-core/src/db/model_rating_prefixes.rs tests.
const time = "2026-08-01T10:20:30Z";
const provider = (id: string, name = id, is_enabled = true): Provider => ({
  id, name, is_enabled, protocol: "openai-compatible", base_url: "https://example.invalid",
  use_proxy: false, fast_mode: false, created_at: time, updated_at: time,
});
const entry = (model_prefix: string, score: number, updated_at = time): ModelRatingEntry => ({
  model_prefix, score, updated_at,
});
const route = (provider_id: string, model: string): Model => ({
  id: "route", name: "alias", balance: "weighted", target_provider: provider_id,
  target_model: model, enable_auth: true, is_enabled: false, created_at: time,
  targets: [{ id: "target", model_id: "route", provider_id, model, weight: 1, priority: 0, created_at: time }],
});
const catalog = (providerId: string, status: ModelCatalogSnapshot["status"], models: string[]): ModelCatalogSnapshot =>
  ({ providerId, status, models });
const filters: ModelRatingFilters = { search: "", rating: "all", min: null, max: null, sort: "score-desc", ratingsReady: true, coverageReady: true };

test("draft validation accepts boundaries and rejects empty, fraction, NaN, exponent, coercion and out of range", () => {
  for (const [input, expected] of [["0", 0], ["100", 100], ["7", 7], ["007", 7]] as const) equal(parseRatingScore(input), expected);
  for (const input of ["", " ", "0.1", "55.0", "NaN", "Infinity", "1e2", "0x64", "-1", "101", "+10", " 50", "50 ", "12abc", "1\n", "９０"]) {
    equal(parseRatingScore(input), null, input);
  }
});

test("prefix validation mirrors backend blank/NUL/1024-byte rules", () => {
  equal(isValidRatingPrefix("deepseek-v4-pro"), true);
  equal(isValidRatingPrefix("模型-é"), true);
  equal(isValidRatingPrefix("x".repeat(1024)), true);
  for (const bad of ["", " ", " \t\n", "a\0b", "x".repeat(1025), "🦀".repeat(257)]) {
    equal(isValidRatingPrefix(bad), false, JSON.stringify(bad));
  }
});

test("canonical prefix lowercases like the backend write path", () => {
  equal(canonicalModelPrefix("DeepSeek-V4-Pro"), "deepseek-v4-pro");
  equal(canonicalModelPrefix("already-lower"), "already-lower");
  equal(canonicalModelPrefix("模型-É"), "模型-é");
});

test("segment-boundary matching mirrors the Rust rules exactly", () => {
  equal(modelMatchesPrefix("deepseek-v4-pro", "deepseek-v4-pro"), true);
  equal(modelMatchesPrefix("deepseek-v4-pro-0813", "deepseek-v4-pro"), true);
  equal(modelMatchesPrefix("deepseek-v4-pro2", "deepseek-v4-pro"), false);
  equal(modelMatchesPrefix("gpt-4o", "gpt-4"), false);
  equal(modelMatchesPrefix("gpt-4-turbo", "gpt-4"), true);
  equal(modelMatchesPrefix("deepseek-v4", "deepseek-v4-pro"), false);
  equal(modelMatchesPrefix("", "deepseek"), false);
  // Case differences are ignored in both directions.
  equal(modelMatchesPrefix("DeepSeek-V4-Pro", "deepseek-v4-pro"), true);
  equal(modelMatchesPrefix("deepseek-v4-pro-0813", "DEEPSEEK-V4-PRO"), true);
  equal(modelMatchesPrefix("GPT-4-TURBO", "gpt-4"), true);
  equal(modelMatchesPrefix("GPT-4O", "gpt-4"), false);
  // Separator identity is still exact.
  equal(modelMatchesPrefix("deepseek v4 pro", "deepseek-v4"), false);
  equal(modelMatchesPrefix("a--b", "a-"), true);
  equal(modelMatchesPrefix("a-b", "a-"), false);
  equal(modelMatchesPrefix("模型-专业-0813", "模型-专业"), true);
  equal(modelMatchesPrefix("模型-x", "模型"), true);
});

test("longest prefix wins overlaps and matchingEntries orders shortest first", () => {
  const entries = [entry("deepseek-v4", 80), entry("deepseek-v4-pro", 92)];
  equal(longestRatingMatch("deepseek-v4-pro-0813", entries)?.model_prefix, "deepseek-v4-pro");
  equal(longestRatingMatch("deepseek-v4-chat", entries)?.model_prefix, "deepseek-v4");
  equal(longestRatingMatch("DeepSeek-V4-PRO-0813", entries)?.model_prefix, "deepseek-v4-pro");
  equal(longestRatingMatch("glm-4.5", entries), null);
  equal(longestRatingMatch("", entries), null);
  deepEqual(matchingEntries("deepseek-v4-pro-0813", entries).map((item) => item.model_prefix), ["deepseek-v4", "deepseek-v4-pro"]);
});

test("list payload failures never masquerade as empty ratings", () => {
  deepEqual(readModelRatings([]), []);
  equal(readModelRatings([entry("p", 0)])[0].score, 0);
  for (const value of [null, {}, { data: [] }, { error: "unsupported" }, [null], [entry("p", 0.5)], [entry("p", -1)], [entry("p", 101)], [{ ...entry("p", 0), score: "0" }], [entry("p", 0, "")], [entry("p", 0, "invalid")], [entry("p", 0, "2026-08-01")], [entry("", 0)]]) {
    throws(() => readModelRatings(value));
  }
  throws(() => readModelRatings([entry("p", 0), entry("p", 50)]));
  equal(readModelRatings([entry("p", 0, "2026-08-01T10:20:30.123456+00:00")]).length, 1);
});

test("display keeps unrated, zero, loading and error distinct per concrete model", () => {
  const entries = [entry("gpt-4", 0), entry("m", 55)];
  equal(ratingDisplayState("ready", "unknown", entries).status, "unrated");
  const zero = ratingDisplayState("ready", "gpt-4o-mini", [entry("gpt-4o", 0)]);
  equal(zero.status, "rated");
  if (zero.status === "rated") equal(zero.entry.score, 0);
  equal(ratingDisplayState("loading", "m", entries).status, "loading");
  equal(ratingDisplayState("error", "m", entries).status, "error");
  equal(ratingDisplayState("ready", "m2", entries).status, "unrated");
});

test("coverage counts models per provider at segment boundaries only", () => {
  const providers = [provider("p", "Alpha"), provider("q", "Beta")];
  const catalogs = [
    catalog("p", "success", ["deepseek-v4-pro", "deepseek-v4-pro-0813", "deepseek-v4-pro2", "other"]),
    catalog("q", "success", ["DeepSeek-V4-Pro"]),
  ];
  const coverage = prefixCoverage("deepseek-v4-pro", catalogs, providers);
  equal(coverage.length, 2);
  deepEqual(coverage[0].models, ["deepseek-v4-pro", "deepseek-v4-pro-0813"]);
  deepEqual(coverage[1].models, ["DeepSeek-V4-Pro"]);
  const rows = buildModelRatingRows([entry("deepseek-v4-pro", 92)], catalogs, providers);
  equal(rows[0].modelCount, 3);
  equal(rows[0].providerCount, 2);
});

test("unmatched union covers catalogs and route targets minus every matched model", () => {
  const providers = [provider("p"), provider("disabled", "Disabled", false)];
  const entries = [entry("rated", 50)];
  const unmatched = buildUnmatchedModels(
    providers,
    [route("p", "mapped"), route("p", "rated-0813"), route("disabled", "hidden"), route("p", "saved-only")],
    [catalog("p", "success", ["catalog-only", "rated"]), catalog("disabled", "success", ["ignored"])],
    entries,
  );
  deepEqual(unmatched.map((row) => [row.provider.id, row.model]), [
    ["p", "catalog-only"], ["p", "mapped"], ["p", "saved-only"],
  ]);
});

test("filtering sorts by score/name/updated with global search and score bounds", () => {
  const providers = [provider("p")];
  const entries = [entry("b", 80), entry("a", 0), entry("c", 90, "2026-08-01T03:00:00+01:00"), entry("d", 90, "2026-08-01T02:00:00Z"), entry("old", 100, "2026-08-01T01:00:00Z")];
  const rows = buildModelRatingRows(entries, [catalog("p", "success", ["a", "b", "c", "d"])], providers);
  deepEqual(filterAndSortModelRatingRows(rows, filters).map((row) => row.entry.model_prefix), ["old", "c", "d", "b", "a"]);
  // b/a share the default 10:20Z instant, c/d both parse to 02:00Z; ties fall to name.
  deepEqual(filterAndSortModelRatingRows(rows, { ...filters, sort: "updated" }).map((row) => row.entry.model_prefix), ["a", "b", "c", "d", "old"]);
  equal(filterAndSortModelRatingRows(rows, { ...filters, min: 80, max: 90 }).length, 3);
  deepEqual(filterAndSortModelRatingRows(rows, { ...filters, search: "A" }).map((row) => row.entry.model_prefix), ["a"]);
  // match-state predicates depend on ready coverage; unknown coverage disables them.
  equal(filterAndSortModelRatingRows(rows, { ...filters, rating: "unmatched" }).length, 1);
  equal(filterAndSortModelRatingRows(rows, { ...filters, rating: "matched" }).length, 4);
  equal(filterAndSortModelRatingRows(rows, { ...filters, rating: "unmatched", coverageReady: false }).length, 5);
  // Unknown scores disable predicates rather than fabricate empty results.
  equal(filterAndSortModelRatingRows(rows, { ...filters, ratingsReady: false, min: 99 }).length, 5);
});

test("global sorting occurs before the first 40-row page", () => {
  const entries = Array.from({ length: 80 }, (_, index) => entry(`model-${String(index).padStart(3, "0")}`, index));
  const rows = buildModelRatingRows(entries, [], []);
  equal(filterAndSortModelRatingRows(rows, filters)[0].entry.score, 79);
  equal(filterAndSortModelRatingRows(rows, { ...filters, min: 60 }).length, 20);
});

test("uniqueModelIdentifiers dedupes catalogs without rewriting identity", () => {
  const models = ["Model", "model", " model", "model ", "model/a?b#c", "model", "模型"];
  const result = uniqueModelIdentifiers(models);
  equal(result.length, 6);
  notEqual(result.indexOf("model "), -1);
  ok(result.includes("模型"));
});
