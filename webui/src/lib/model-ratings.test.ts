import { deepEqual, equal, notEqual, throws } from "node:assert/strict";
import { test } from "node:test";
import { buildModelRatingRows, filterAndSortModelRatingRows, countRatingOverrides, emptyRatingProfileInput, hasModelRating, isProviderModelRatingProfile, parseRatingProfileDraft, parseRatingScore, providerModelKey, ratingForDimension, ratingProfileDraft, ratingDisplayState, readProviderModelRatingProfiles, uniqueModelIdentifiers, type ModelRatingFilters } from "./model-ratings";
import { EFFORT_TIERS, type Model, type Provider, type ProviderModelRatingProfile } from "./types";

// No test framework needed: compile this file with tsc --module commonjs into a temporary
// directory outside webui, then run node --test <temp>/model-ratings.test.js.
const time = "2026-08-01T10:20:30Z";
const provider = (id: string, name = id, is_enabled = true): Provider => ({
  id, name, is_enabled, protocol: "openai-compatible", base_url: "https://example.invalid",
  use_proxy: false, fast_mode: false, created_at: time, updated_at: time,
});
const rating = (provider_id: string, upstream_model: string, score: number, updated_at = time): ProviderModelRatingProfile => ({
  provider_id, upstream_model, common: { score, updated_at },
  overrides: { low: null, medium: null, high: null, xhigh: null, max: null },
  effective: Object.fromEntries(EFFORT_TIERS.map((tier) => [tier, { status: "rated", score, source: "common", score_updated_at: updated_at }])) as ProviderModelRatingProfile["effective"],
  display_mode: "common",
});
const route = (provider_id: string, model: string): Model => ({
  id: "route", name: "alias", balance: "weighted", target_provider: provider_id,
  target_model: model, enable_auth: true, is_enabled: false, created_at: time,
  targets: [{ id: "target", model_id: "route", provider_id, model, weight: 1, priority: 0, created_at: time }],
});
const filters: ModelRatingFilters = { search: "", providerId: null, rating: "all", min: null, max: null, sort: "score-desc", ratingsReady: true };

test("draft validation accepts boundaries and rejects empty, fraction, NaN, exponent, coercion and out of range", () => {
  for (const [input, expected] of [["0", 0], ["100", 100], ["7", 7], ["007", 7]] as const) equal(parseRatingScore(input), expected);
  for (const input of ["", " ", "0.1", "55.0", "NaN", "Infinity", "1e2", "0x64", "-1", "101", "+10", " 50", "50 ", "12abc", "1\n", "９０"]) {
    equal(parseRatingScore(input), null, input);
  }
});

test("provider-model identity and catalog dedup preserve raw model bytes", () => {
  const models = ["Model", "model", " model", "model ", "model/a?b#c", "model", "模型"];
  const result = uniqueModelIdentifiers(models);
  equal(result.length, 6);
  for (const value of new Set(models)) equal(result.includes(value), true);
  notEqual(providerModelKey("p", "model"), providerModelKey("p", " model"));
  notEqual(providerModelKey("p", "model"), providerModelKey("q", "model"));
  notEqual(providerModelKey("a\u0000b", "c"), providerModelKey("a", "b\u0000c"));
});

test("list payload failures never masquerade as empty ratings", () => {
  deepEqual(readProviderModelRatingProfiles([]), []);
  equal(readProviderModelRatingProfiles([rating("p", "m", 0)])[0].common?.score, 0);
  for (const value of [null, {}, { data: [] }, { error: "unsupported" }, [null], [rating("p", "m", 0.5)], [rating("p", "m", -1)], [rating("p", "m", 101)], [{ ...rating("p", "m", 0), common: { score: "0", updated_at: time } }], [rating("p", "m", 0, "")], [rating("p", "m", 0, "invalid")], [rating("p", "m", 0, "2026-08-01")]]) {
    throws(() => readProviderModelRatingProfiles(value));
  }
  throws(() => readProviderModelRatingProfiles([rating("p", "m", 0), rating("p", "m", 50)]));
  equal(readProviderModelRatingProfiles([rating("p", "m", 0, "2026-08-01T10:20:30.123456+00:00")]).length, 1);
});

test("display keeps unrated, zero, loading, error and unknown provider distinct", () => {
  equal(ratingDisplayState("ready").status, "unrated");
  const zero = ratingDisplayState("ready", rating("p", "m", 0));
  equal(zero.status, "rated");
  if (zero.status === "rated") equal(zero.rating.common?.score, 0);
  equal(ratingDisplayState("loading").status, "loading");
  equal(ratingDisplayState("error", rating("p", "m", 0)).status, "error");
  equal(ratingDisplayState("ready", null, false).status, "error");
});

test("union includes exact catalogs, disabled route references and saved-only missing/disabled models", () => {
  const rows = buildModelRatingRows(
    [provider("p"), provider("disabled", "Disabled", false), provider("failure")],
    [route("p", "mapped"), route("p", " name "), route("disabled", "route-only"), route("failure", "not-returned"), route("unknown", "model")],
    [rating("p", "name", 0), rating("p", "saved-only", 88), rating("disabled", "saved", 45)],
    [
      { providerId: "p", status: "success", models: ["name", " name ", "name"] },
      { providerId: "disabled", status: "success", models: ["cached"] },
      { providerId: "failure", status: "unknown", models: ["stale"] },
    ],
  );
  const byKey = new Map(rows.map((row) => [row.key, row]));
  equal(rows.length, 8);
  equal(byKey.get(providerModelKey("p", "name"))?.rating?.common?.score, 0);
  equal(byKey.get(providerModelKey("p", " name "))?.catalogStatus, "listed");
  equal(byKey.get(providerModelKey("p", "mapped"))?.catalogStatus, "missing");
  equal(byKey.get(providerModelKey("p", "saved-only"))?.catalogStatus, "missing");
  equal(byKey.get(providerModelKey("disabled", "saved"))?.catalogStatus, "unknown");
  equal(byKey.get(providerModelKey("failure", "not-returned"))?.catalogStatus, "unknown");
  equal(byKey.get(providerModelKey("unknown", "model"))?.catalogStatus, "unknown");
  equal(byKey.has(providerModelKey("disabled", "cached")), false);
  equal(byKey.has(providerModelKey("failure", "stale")), false);
});

test("successful empty catalog means missing, failed catalog means unknown", () => {
  for (const [status, expected] of [["success", "missing"], ["unknown", "unknown"], ["loading", "unknown"]] as const) {
    const [row] = buildModelRatingRows([provider("p")], [], [rating("p", "saved", 50)], [{ providerId: "p", status, models: [] }]);
    equal(row.catalogStatus, expected);
  }
});

test("clearing saved-only records removes only the orphan union row", () => {
  const providers = [provider("p")];
  const routes = [route("p", "mapped")];
  const before = buildModelRatingRows(providers, routes, [rating("p", "saved-only", 80)], []);
  equal(before.length, 2);
  const after = buildModelRatingRows(providers, routes, [], []);
  deepEqual(after.map((row) => row.model), ["mapped"]);
});

test("score sort directions keep unrated last and use stable provider/name/id ties", () => {
  const rows = buildModelRatingRows(
    [provider("z", "Alpha"), provider("a", "Alpha"), provider("b", "Beta")],
    [route("a", "unrated")],
    [rating("b", "m", 80), rating("z", "m", 80), rating("a", "z", 80), rating("a", "m", 80), rating("a", "zero", 0)], [],
  );
  const sorted = filterAndSortModelRatingRows(rows, filters);
  deepEqual(sorted.map((row) => [row.providerId, row.model]), [["a", "m"], ["z", "m"], ["a", "z"], ["b", "m"], ["a", "zero"], ["a", "unrated"]]);
  const asc = filterAndSortModelRatingRows(rows, { ...filters, sort: "score-asc" });
  equal(asc[0].rating?.common?.score, 0);
  equal(asc[asc.length - 1]?.model, "unrated");
  deepEqual(filterAndSortModelRatingRows([...rows].reverse(), filters).map((row) => row.key), sorted.map((row) => row.key));
});

test("rating and min/max predicates exclude unscored; text search doesn't alter exact identity", () => {
  const rows = buildModelRatingRows([provider("p", "Alpha"), provider("q")], [route("p", "unknown")], [rating("p", " Model / 1 ", 0), rating("q", "other", 90)], []);
  equal(filterAndSortModelRatingRows(rows, { ...filters, min: 0 }).length, 2);
  deepEqual(filterAndSortModelRatingRows(rows, { ...filters, min: 0, max: 0 }).map((row) => row.model), [" Model / 1 "]);
  equal(filterAndSortModelRatingRows(rows, { ...filters, rating: "unrated" }).length, 1);
  equal(filterAndSortModelRatingRows(rows, { ...filters, rating: "rated" }).length, 2);
  equal(filterAndSortModelRatingRows(rows, { ...filters, rating: "unrated", min: 0 }).length, 0);
  deepEqual(filterAndSortModelRatingRows(rows, { ...filters, search: "model-1", providerId: "p" }).map((row) => row.model), [" Model / 1 "]);
});

test("unknown ratings disable score predicates and sorting rather than fabricate empty results", () => {
  const rows = buildModelRatingRows([provider("p"), provider("q")], [route("p", "unknown")], [rating("q", "rated", 90)], []);
  const result = filterAndSortModelRatingRows(rows, { ...filters, ratingsReady: false, rating: "rated", min: 99 });
  equal(result.length, 2);
  equal(result[0].model, "unknown");
  equal(filterAndSortModelRatingRows(rows, { ...filters, ratingsReady: false, providerId: "p" }).length, 1);
});

test("global filtering and sorting occur before the first 40-row page", () => {
  const saved = Array.from({ length: 80 }, (_, index) => rating("p", `model-${index}`, index));
  const rows = buildModelRatingRows([provider("p")], [], saved, []);
  const sorted = filterAndSortModelRatingRows(rows, filters);
  equal(sorted.slice(0, 40)[0].rating?.common?.score, 79);
  const filtered = filterAndSortModelRatingRows(rows, { ...filters, min: 60 });
  equal(filtered.length, 20);
  equal(filtered.slice(0, 40)[19]?.rating?.common?.score, 60);
});

test("profile drafts preserve equal overrides and independent common removal with all five keys", () => {
  const profile = rating("p", "m", 70);
  profile.overrides.low = { score: 70, updated_at: time };
  profile.display_mode = "per_effort";
  const draft = ratingProfileDraft(profile);
  equal(draft.overrides.low.enabled, true);
  equal(parseRatingProfileDraft(draft)?.overrides.low, 70);
  draft.commonEnabled = false;
  deepEqual(parseRatingProfileDraft(draft), { common: null, overrides: { low: 70, medium: null, high: null, xhigh: null, max: null } });
  draft.overrides.max.enabled = true;
  draft.overrides.max.score = "";
  equal(parseRatingProfileDraft(draft), null);
  draft.overrides.max.score = "100";
  equal(parseRatingProfileDraft(draft)?.overrides.max, 100);
  draft.overrides.low.enabled = false;
  equal(parseRatingProfileDraft(draft)?.overrides.low, null);
  deepEqual(emptyRatingProfileInput(), { common: null, overrides: { low: null, medium: null, high: null, xhigh: null, max: null } });
  equal(parseRatingProfileDraft(ratingProfileDraft(null))?.common, null);
});

test("override-only profiles are rated badges but not common scores; effective values drive each dimension", () => {
  const profile = rating("p", "m", 70);
  profile.common = null;
  profile.overrides.high = { score: 99, updated_at: time };
  profile.display_mode = "per_effort";
  for (const tier of EFFORT_TIERS) profile.effective[tier] = { status: "unrated", score: null, source: "unrated", score_updated_at: null };
  profile.effective.high = { status: "rated", score: 39, source: "override", score_updated_at: time };
  equal(isProviderModelRatingProfile(profile), true); // Backend effective is authoritative, not recomputed.
  equal(hasModelRating(profile), true);
  equal(countRatingOverrides(profile), 1);
  equal(ratingDisplayState("ready", profile).status, "rated");
  equal(ratingForDimension(profile, "common").status, "unrated");
  equal(ratingForDimension(profile, "high").score, 39);
  const rows = buildModelRatingRows([provider("p")], [], [profile, rating("p", "other", 50)], []);
  equal(filterAndSortModelRatingRows(rows, { ...filters, rating: "rated" }).length, 1);
  equal(filterAndSortModelRatingRows(rows, { ...filters, dimension: "high", min: 39, max: 39 })[0]?.model, "m");
  equal(filterAndSortModelRatingRows(rows, { ...filters, dimension: "high", sort: "score-asc" })[0]?.model, "m");
  equal(filterAndSortModelRatingRows(rows, { ...filters, dimension: "low", rating: "unrated" })[0]?.model, "m");
});

test("profile validation requires all five overrides/effective fields and rejects malformed domains", () => {
  const profile = rating("p", "m", 0);
  for (const tier of EFFORT_TIERS) {
    const missingOverride = { ...profile.overrides } as Partial<ProviderModelRatingProfile["overrides"]>;
    delete missingOverride[tier];
    equal(isProviderModelRatingProfile({ ...profile, overrides: missingOverride }), false);
    const missingEffective = { ...profile.effective } as Partial<ProviderModelRatingProfile["effective"]>;
    delete missingEffective[tier];
    equal(isProviderModelRatingProfile({ ...profile, effective: missingEffective }), false);
  }
  for (const bad of [
    { ...profile, display_mode: "minimal" },
    { ...profile, effective: { ...profile.effective, low: { status: "rated", score: null, source: "common", score_updated_at: time } } },
    { ...profile, effective: { ...profile.effective, low: { status: "unrated", score: 0, source: "unrated", score_updated_at: null } } },
    { ...profile, overrides: { ...profile.overrides, max: { score: 101, updated_at: time } } },
  ]) equal(isProviderModelRatingProfile(bad), false);
  const empty = { ...profile, common: null, effective: Object.fromEntries(EFFORT_TIERS.map((tier) => [tier, { status: "unrated", score: null, source: "unrated", score_updated_at: null }])) } as ProviderModelRatingProfile;
  equal(isProviderModelRatingProfile(empty), true);
  equal(ratingDisplayState("ready", empty).status, "unrated");
});

test("updated sort compares instants, preserves deterministic ties, and leaves unrated last", () => {
  const rows = buildModelRatingRows([provider("p")], [route("p", "unrated")], [
    rating("p", "old", 100, "2026-08-01T01:00:00Z"),
    rating("p", "b", 0, "2026-08-01T03:00:00+01:00"),
    rating("p", "a", 0, "2026-08-01T02:00:00Z"),
  ], []);
  deepEqual(filterAndSortModelRatingRows(rows, { ...filters, sort: "updated" }).map((row) => row.model), ["a", "b", "old", "unrated"]);
  deepEqual(filterAndSortModelRatingRows(rows, { ...filters, sort: "name" }).map((row) => row.model), ["a", "b", "old", "unrated"]);
});
