import { deepEqual, equal, throws } from "node:assert/strict";
import { test } from "node:test";
import {
  failedProbeModels, loadModelProbeResults, mergeModelProbeRun, probeResultAccountId,
  probeResultsForScope, saveModelProbeResults,
} from "./model-probe";
import type { ModelProbeMeta, ModelProbeResult } from "./types";

const result = (model: string, success: boolean): ModelProbeResult => ({ model, success, latency_ms: 3, protocol: "test" });
const meta = (account_id?: string, account_label?: string): ModelProbeMeta => ({ protocol: "test", base_url: "https://example.test", account_id, account_label });
const oldRun = "2026-08-01T10:00:00Z";
const newRun = "2026-08-01T11:00:00Z";

test("same model and label in distinct accounts never merge, or overwrite provider history", () => {
  const legacy = mergeModelProbeRun(undefined, [result("shared", true), result("legacy-fail", false)], oldRun);
  const a = mergeModelProbeRun(legacy, [result("shared", false)], newRun, meta("a", "Work"), "a");
  const b = mergeModelProbeRun(a, [result("shared", true)], newRun, meta("b", "Work"), "b");
  deepEqual(b.results, legacy.results);
  equal(b.tested_at, oldRun);
  equal(probeResultsForScope(b, "a")[0].success, false);
  equal(probeResultsForScope(b, "b")[0].success, true);
  deepEqual(probeResultsForScope(b, "untested"), []);
  equal(b.accounts?.a.account_label, "Work");
  const providerRetry = mergeModelProbeRun(b, [result("shared", false)], newRun);
  equal(probeResultsForScope(providerRetry)[1].success, false);
  deepEqual(providerRetry.accounts, b.accounts, "legacy runs keep all account history");
});

test("account subset merges preserve untouched models and reject superseded timestamps and labels", () => {
  const initial = mergeModelProbeRun(undefined, [result("a", true), result("b", false)], newRun, meta("id", "New"), "id");
  const late = mergeModelProbeRun(initial, [result("a", false), result("c", true)], oldRun, meta("id", "Old"), "id");
  deepEqual(probeResultsForScope(late), [], "account results never become global failures");
  deepEqual(probeResultsForScope(late, "id").map((r) => [r.model, r.success]), [["a", true], ["b", false], ["c", true]]);
  equal(late.accounts?.id.account_label, "New");
  equal(late.accounts?.id.tested_at, newRun);
});

test("explicit probes require matching backend identity; unscoped legacy responses remain compatible", () => {
  equal(probeResultAccountId(undefined), undefined);
  equal(probeResultAccountId(meta()), undefined);
  equal(probeResultAccountId(meta("actual")), "actual");
  equal(probeResultAccountId(meta("actual"), "actual"), "actual");
  throws(() => mergeModelProbeRun(undefined, [result("a", false)], newRun, meta("other"), "selected"), /does not match/);
  throws(() => mergeModelProbeRun(undefined, [result("a", false)], newRun, meta(), "selected"), /did not confirm/);
  throws(() => probeResultAccountId(undefined, "selected"), /did not confirm/);
});

test("persisted account failures stay out of failedProbeModels; legacy records survive load/save", () => {
  const original = Object.getOwnPropertyDescriptor(globalThis, "window");
  const storage = new Map<string, string>();
  Object.defineProperty(globalThis, "window", { configurable: true, value: { localStorage: {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => { storage.set(key, value); },
  } } });
  try {
    const legacy = { tested_at: oldRun, results: [result("legacy-fail", false)] };
    const scoped = mergeModelProbeRun(legacy, [result("account-fail", false)], newRun, meta("a", "A"), "a");
    const other = mergeModelProbeRun(undefined, [result("other-fail", false)], newRun, meta("a", "Same ID"), "a");
    saveModelProbeResults({ provider: scoped, other, legacy });
    const loaded = loadModelProbeResults();
    deepEqual([...failedProbeModels("provider")], ["legacy-fail"]);
    deepEqual([...failedProbeModels("other")], []);
    deepEqual([...failedProbeModels("legacy")], ["legacy-fail"]);
    deepEqual(probeResultsForScope(loaded.provider, "a").map((r) => r.model), ["account-fail"]);
    deepEqual(probeResultsForScope(loaded.other, "a").map((r) => r.model), ["other-fail"]);
    equal(loaded.provider.accounts?.a.account_label, "A");
    equal(loaded.legacy.results[0].run_at, undefined);
    // Corrupt scope identity must not be relabeled by a dictionary key.
    storage.set("nyro.providerModelProbe.v1", JSON.stringify({ bad: { ...legacy, accounts: {
      a: { account_id: "b", ...legacy },
    } } }));
    equal(loadModelProbeResults().bad.accounts, undefined);
  } finally {
    if (original) Object.defineProperty(globalThis, "window", original);
    else Reflect.deleteProperty(globalThis, "window");
  }
});
