/**
 * Model-probe persistence — shared between the providers page (writes probe
 * results) and the models page (filters unreachable models from route target
 * pickers). Stored in localStorage so no backend schema change is needed.
 */

import type { ModelProbeResult } from "@/lib/types";

const MODEL_PROBE_STORAGE_KEY = "nyro.providerModelProbe.v1";

export interface ProviderModelProbeRecord {
  results: ModelProbeResult[];
  tested_at: string;
}

export type ProviderModelProbeStore = Record<string, ProviderModelProbeRecord>;

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
        results: record.results.filter(
          (result) =>
            result
            && typeof result === "object"
            && typeof result.model === "string"
            && typeof result.success === "boolean",
        ),
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
