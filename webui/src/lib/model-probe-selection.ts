/**
 * Probe-selection helpers for the model-probe picker: a one-shot selection of
 * the models a probe run should hit, remembered per provider in localStorage
 * (no backend schema change). The remembered list is flat — one name occupies
 * one place, and reopening the picker partitions it by current catalog
 * membership (see `partitionSavedSelection`).
 */

const MODEL_PROBE_SELECTION_STORAGE_KEY = "nyro.providerModelProbeSelection.v1";
const MODEL_PROBE_PROMPT_STORAGE_KEY = "nyro.providerModelProbePrompt.v1";

/** Built-in probe message used when the prompt box is left blank. */
export const DEFAULT_MODEL_PROBE_PROMPT = "hi";

/** Mirrors the server-side cap (`MAX_MODEL_PROBE_PROMPT_CHARS`). */
export const MAX_MODEL_PROBE_PROMPT_CHARS = 4000;

/** Trim the custom probe message; blank means "use the built-in default". */
export function normalizeProbePrompt(prompt: string): string {
  return prompt.trim();
}

/** Short single-line label of the message a probe run sends, for logs. */
export function probePromptLabel(prompt: string, max = 60): string {
  const flat = normalizeProbePrompt(prompt).replace(/\s+/g, " ") || DEFAULT_MODEL_PROBE_PROMPT;
  return flat.length > max ? `${flat.slice(0, max)}…` : flat;
}

/** The custom probe message is shared by every provider (last one used). */
export function loadProbePrompt(): string {
  if (typeof window === "undefined") return "";
  try {
    return window.localStorage.getItem(MODEL_PROBE_PROMPT_STORAGE_KEY) ?? "";
  } catch {
    return "";
  }
}

export function saveProbePrompt(prompt: string): void {
  if (typeof window === "undefined") return;
  try {
    const value = normalizeProbePrompt(prompt);
    if (value) window.localStorage.setItem(MODEL_PROBE_PROMPT_STORAGE_KEY, value);
    else window.localStorage.removeItem(MODEL_PROBE_PROMPT_STORAGE_KEY);
  } catch {
    // Ignore storage errors; the picker just falls back to the default.
  }
}

/** Trim, drop empty entries and exact duplicates; keep first-seen order. */
export function normalizeModelList(names: readonly string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const name of names) {
    const trimmed = name.trim();
    if (!trimmed || seen.has(trimmed)) continue;
    seen.add(trimmed);
    out.push(trimmed);
  }
  return out;
}

/** Split the free-form extras box (one per line or comma separated). */
export function splitExtraModels(text: string): string[] {
  return normalizeModelList(text.split(/[\n,]/));
}

/**
 * One name occupies one place: remembered names that are in the current
 * catalog come back as checked checkboxes; the rest go to the extras box.
 * With an unavailable/empty catalog every name lands in the extras box.
 */
export function partitionSavedSelection(
  saved: readonly string[],
  catalog: readonly string[],
): { checked: string[]; extras: string[] } {
  const inCatalog = new Set(catalog.map((name) => name.trim()));
  const checked = new Set<string>();
  const extras: string[] = [];
  for (const name of normalizeModelList(saved)) {
    if (inCatalog.has(name)) checked.add(name);
    else extras.push(name);
  }
  return { checked: [...checked], extras };
}

/** Final probe list: checked models ∪ extras box, normalized and deduped. */
export function buildProbeSelection(
  checked: readonly string[],
  extrasText: string,
): string[] {
  return normalizeModelList([...checked, ...splitExtraModels(extrasText)]);
}

export function loadProbeSelection(providerId: string): string[] {
  if (typeof window === "undefined") return [];
  try {
    const raw = window.localStorage.getItem(MODEL_PROBE_SELECTION_STORAGE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    const entry = parsed?.[providerId];
    if (!Array.isArray(entry)) return [];
    return normalizeModelList(entry.filter((value): value is string => typeof value === "string"));
  } catch {
    return [];
  }
}

export function saveProbeSelection(providerId: string, names: readonly string[]): void {
  if (typeof window === "undefined") return;
  try {
    const raw = window.localStorage.getItem(MODEL_PROBE_SELECTION_STORAGE_KEY);
    let parsed: Record<string, unknown> = {};
    if (raw) {
      const value = JSON.parse(raw) as unknown;
      if (value && typeof value === "object") parsed = value as Record<string, unknown>;
    }
    const next = { ...parsed, [providerId]: normalizeModelList(names) };
    window.localStorage.setItem(MODEL_PROBE_SELECTION_STORAGE_KEY, JSON.stringify(next));
  } catch {
    // Ignore storage errors; the picker just starts unchecked next time.
  }
}
