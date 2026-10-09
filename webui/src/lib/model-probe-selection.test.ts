import { deepEqual, equal, ok } from "node:assert/strict";
import { test } from "node:test";
import {
  buildProbeSelection,
  normalizeModelList,
  partitionSavedSelection,
  splitExtraModels,
} from "./model-probe-selection";
import { mergeProbeResults, type ProviderModelProbeRecord } from "./model-probe";
import type { ModelProbeResult } from "./types";

// No test framework needed: compile this file with tsc --module commonjs into a temporary
// directory outside webui, then run node --test <temp>/model-probe-selection.test.js.

const result = (model: string, success = true): ModelProbeResult => ({
  model,
  success,
  latency_ms: 42,
  protocol: "openai-compatible/chat-completions/v1",
});

test("normalize trims, drops blanks and exact duplicates in first-seen order", () => {
  deepEqual(normalizeModelList([" b ", "a", "b", "", "   ", "c", "a"]), ["b", "a", "c"]);
});

test("extras split on newlines and commas only", () => {
  deepEqual(splitExtraModels("m1, m2\nm3,,\n  m1"), ["m1", "m2", "m3"]);
});

test("one name occupies one place: catalog names become checkboxes, the rest extras", () => {
  const { checked, extras } = partitionSavedSelection(
    ["m2", "ghost", "m1", "m2", "other"],
    ["m1", "m2", "m3"],
  );
  deepEqual(checked, ["m2", "m1"]);
  deepEqual(extras, ["ghost", "other"]);
});

test("every remembered name lands in the extras box when the catalog is unavailable", () => {
  const { checked, extras } = partitionSavedSelection(["m1", "ghost"], []);
  deepEqual(checked, []);
  deepEqual(extras, ["m1", "ghost"]);
});

test("probe selection is the checked ∪ extras union, deduped", () => {
  deepEqual(buildProbeSelection(["m1", "m2"], "m2\nghost, m1"), ["m1", "m2", "ghost"]);
  equal(buildProbeSelection([], " \n , ").length, 0);
});

test("a run only overwrites the models it reported", () => {
  const previous: ProviderModelProbeRecord = {
    tested_at: "2026-08-01T10:00:00Z",
    results: [
      { ...result("a", true), run_at: "2026-08-01T10:00:00Z" },
      { ...result("b", true), run_at: "2026-08-01T10:00:00Z" },
    ],
  };
  const merged = mergeProbeResults(previous, [result("b", false)], "2026-08-01T11:00:00Z");
  equal(merged.results.length, 2, "unprobed models keep their state");
  equal(merged.results.find((entry) => entry.model === "a")?.success, true);
  equal(merged.results.find((entry) => entry.model === "b")?.success, false);
});

test("a late superseded run never overwrites a newer result", () => {
  const newer = mergeProbeResults(undefined, [result("a", true)], "2026-08-01T11:00:00Z");
  const late = mergeProbeResults(newer, [result("a", false)], "2026-08-01T10:30:00Z");
  equal(late.results.find((entry) => entry.model === "a")?.success, true, "newer run wins");
  equal(late.tested_at, "2026-08-01T11:00:00Z", "late run does not roll tested_at back");

  // …but the late run still fills models the newer run never touched.
  const filled = mergeProbeResults(newer, [result("b", false)], "2026-08-01T10:30:00Z");
  equal(filled.results.length, 2);
  equal(filled.results.find((entry) => entry.model === "b")?.success, false);
});

test("entries without run_at count as the oldest run", () => {
  const legacy: ProviderModelProbeRecord = {
    tested_at: "",
    results: [{ ...result("a", true) }],
  };
  const merged = mergeProbeResults(legacy, [result("a", false)], "2026-08-01T10:00:00Z");
  const entry = merged.results.find((item) => item.model === "a");
  equal(entry?.success, false, "legacy entries are older than any run");
  equal(entry?.run_at, "2026-08-01T10:00:00Z");
  ok(merged.results.every((item) => typeof item.model === "string"));
});
