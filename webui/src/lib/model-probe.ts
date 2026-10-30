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

import type { ModelProbeMeta, ModelProbeResult } from "./types";

const MODEL_PROBE_STORAGE_KEY = "nyro.providerModelProbe.v1";

/** Persisted probe result: every entry records the run it came from. */
export type StoredModelProbeResult = ModelProbeResult & { run_at?: string };

export interface ModelProbeRecord {
  results: StoredModelProbeResult[];
  tested_at: string;
}

export interface AccountModelProbeRecord extends ModelProbeRecord {
  account_id: string;
  account_label?: string;
}

export interface ProviderModelProbeRecord extends ModelProbeRecord {
  /** Account results never enter provider-level results or global model filtering. */
  accounts?: Record<string, AccountModelProbeRecord>;
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
  previous: ModelProbeRecord | undefined,
  results: ModelProbeResult[],
  runAt: string,
): ModelProbeRecord {
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

/** A pinned run must prove which account ran; older auto-scheduled responses are unsafe. */
export function probeResultAccountId(meta: ModelProbeMeta | undefined, requestedAccountId?: string): string | undefined {
  const reported = meta?.account_id || undefined;
  if (requestedAccountId && requestedAccountId !== reported) {
    throw new Error(reported
      ? "Probe response account does not match the selected account"
      : "Probe response did not confirm the selected account; update the server and retry");
  }
  return reported;
}

/** Merge only within one provider/account namespace, never between accounts. */
export function mergeModelProbeRun(
  previous: ProviderModelProbeRecord | undefined,
  results: ModelProbeResult[],
  runAt: string,
  meta?: ModelProbeMeta,
  requestedAccountId?: string,
): ProviderModelProbeRecord {
  const accountId = probeResultAccountId(meta, requestedAccountId);
  if (!accountId) return { ...previous, ...mergeProbeResults(previous, results, runAt) };
  const existing = previous?.accounts?.[accountId];
  const label = !existing || existing.tested_at <= runAt
    ? meta?.account_label || existing?.account_label
    : existing.account_label;
  return {
    results: previous?.results ?? [],
    tested_at: previous?.tested_at ?? "",
    accounts: {
      ...previous?.accounts,
      [accountId]: {
        ...mergeProbeResults(existing, results, runAt),
        account_id: accountId,
        ...(label ? { account_label: label } : {}),
      },
    },
  };
}

/** An account without history is untested, not a fallback to provider history. */
export function probeResultsForScope(record: ProviderModelProbeRecord | undefined, accountId?: string): StoredModelProbeResult[] {
  return (accountId ? record?.accounts?.[accountId]?.results : record?.results) ?? [];
}

function normalizeRecord(record: ModelProbeRecord): ModelProbeRecord {
  return {
    tested_at: typeof record.tested_at === "string" ? record.tested_at : "",
    results: record.results.filter((result) => result && typeof result === "object"
      && typeof result.model === "string" && typeof result.success === "boolean")
      .map((result) => ({ ...result, run_at: typeof result.run_at === "string" ? result.run_at : undefined })),
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
      const accounts: Record<string, AccountModelProbeRecord> = {};
      if (record.accounts && typeof record.accounts === "object") {
        for (const [accountId, account] of Object.entries(record.accounts)) {
          if (!account || account.account_id !== accountId || !Array.isArray(account.results)) continue;
          accounts[accountId] = {
            ...normalizeRecord(account),
            account_id: accountId,
            ...(typeof account.account_label === "string" ? { account_label: account.account_label } : {}),
          };
        }
      }
      normalized[id] = {
        ...normalizeRecord(record),
        ...(Object.keys(accounts).length ? { accounts } : {}),
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

/** Only provider-level failures hide models globally; account failures never do. */
export function failedProbeModels(providerId: string): Set<string> {
  const results = probeResultsForScope(loadModelProbeResults()[providerId]);
  return new Set(results.filter((result) => !result.success).map((result) => result.model));
}
