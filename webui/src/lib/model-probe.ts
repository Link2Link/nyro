/**
 * Model-probe persistence — shared between the pages that write probe results
 * (providers, available models) and the pages that filter unreachable models
 * from route target pickers (models). Stored in localStorage so no backend
 * schema change is needed.
 *
 * Records merge, never wholesale-replace: per model the result from the newer
 * probe run wins (see `mergeProbeResults`), and models a run did not touch
 * keep their previous state.
 */

import type { ModelProbeResult } from "./types";

const MODEL_PROBE_STORAGE_KEY = "nyro.providerModelProbe.v1";

/** Persisted probe result: every entry records the run it came from. */
export type StoredModelProbeResult = ModelProbeResult & { run_at?: string };

export interface ProviderModelProbeRecord {
  results: StoredModelProbeResult[];
  tested_at: string;
}

export type ProviderModelProbeStore = Record<string, ProviderModelProbeRecord>;

/**
 * Merge one probe run into the previous record. Per model the run with the
 * newer `run_at` wins, so a superseded run that answers late can never
 * overwrite the result of a later retry; entries from records written before
 * run timestamps existed count as the oldest run. Models the run did not
 * report keep their previous state.
 */
export function mergeProbeResults(
  previous: ProviderModelProbeRecord | undefined,
  results: ModelProbeResult[],
  runAt: string,
): ProviderModelProbeRecord {
  const byModel = new Map<string, StoredModelProbeResult>();
  for (const result of previous?.results ?? []) {
    if (result && typeof result.model === "string") byModel.set(result.model, result);
  }
  for (const result of results) {
    const existing = byModel.get(result.model);
    if (existing && (existing.run_at ?? "") > runAt) continue;
    byModel.set(result.model, { ...result, run_at: runAt });
  }
  const previousTestedAt = previous?.tested_at ?? "";
  return {
    results: [...byModel.values()].sort((a, b) => a.model.localeCompare(b.model)),
    tested_at: previousTestedAt > runAt ? previousTestedAt : runAt,
  };
}

export function loadModelProbeResults(): ProviderModelProbeStore {
  if (typeof window === "undefined") return {};
  try {
    const raw = window.localStorage.getItem(MODEL_PROBE_STORAGE_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as ProviderModelProbeStore;
    if (!parsed || typeof parsed !== "object") return {};
    const normalized: ProviderModelProbeStore = {};
    for (const [id, record] of Object.entries(parsed)) {
      if (!record || typeof record !== "object" || !Array.isArray(record.results)) continue;
      normalized[id] = {
        tested_at: typeof record.tested_at === "string" ? record.tested_at : "",
        results: record.results
          .filter(
            (result) =>
              result
              && typeof result === "object"
              && typeof result.model === "string"
              && typeof result.success === "boolean",
          )
          .map((result) => ({
            ...result,
            run_at: typeof result.run_at === "string" ? result.run_at : undefined,
          })),
      };
    }
    return normalized;
  } catch {
    return {};
  }
}

export function saveModelProbeResults(results: ProviderModelProbeStore) {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(MODEL_PROBE_STORAGE_KEY, JSON.stringify(results));
  } catch {
    // Ignore storage errors to avoid breaking provider UI.
  }
}

/** Models whose latest probe failed for the given provider. */
export function failedProbeModels(providerId: string): Set<string> {
  const record = loadModelProbeResults()[providerId];
  if (!record) return new Set();
  return new Set(record.results.filter((result) => !result.success).map((result) => result.model));
}
