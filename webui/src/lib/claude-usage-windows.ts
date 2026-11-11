import type { ClaudeUsageWindow, ProviderAccountView } from "./devin-account-pool";

/** Matches the backend horizon: one active refresh interval plus slack. */
export const CLAUDE_USAGE_FRESH_MS = 660_000;
/** Tolerated client/server clock skew for observation timestamps. */
const CLOCK_SKEW_MS = 60_000;
const MAIN_WINDOWS = ["five_hour", "weekly_limit"];

export type ClaudeWindowSource = "usage_api" | "inference_header";

/** Display state of one effective Claude usage window. */
export interface ClaudeWindowView {
  name: string;
  /** Recorded percentage; null when invalid (never inferred, never 0 by default). */
  used: number | null;
  /** Parseable reset time (ISO) or null. */
  reset: string | null;
  /** null when the response carries no per-window provenance. */
  source: ClaudeWindowSource | null;
  /** Valid observation time (epoch ms) or null when unknown. */
  observedAt: number | null;
  /** Reset already passed: the value belongs to a finished cycle. */
  expired: boolean;
  fresh: boolean;
}

export const isClaudeMainWindow = (name: string) => MAIN_WINDOWS.includes(name);

/** A positive, representable epoch-ms timestamp that is not in the future. */
export function validObservedAt(value: number | null | undefined, now: number): number | null {
  return value != null && Number.isFinite(value) && value > 0 && value <= now + CLOCK_SKEW_MS
    && Number.isFinite(new Date(value).getTime()) ? value : null;
}

/**
 * Header-sourced windows carry their own observation time; full-query windows
 * also depend on the account's full-query health (`stale`).
 */
export function claudeWindowView(
  window: ClaudeUsageWindow,
  account: Pick<ProviderAccountView, "stale" | "observed_at">,
  now: number,
): ClaudeWindowView {
  const source: ClaudeWindowSource = window.source === "inference_header" ? "inference_header" : "usage_api";
  const observedAt = validObservedAt(source === "inference_header" ? window.observed_at : window.observed_at ?? account.observed_at, now);
  const resetMs = window.resets_at ? Date.parse(window.resets_at) : NaN;
  const reset = Number.isFinite(resetMs) ? window.resets_at! : null;
  const expired = reset != null && resetMs <= now;
  const used = Number.isFinite(window.used_percent) && window.used_percent >= 0 && window.used_percent <= 100
    ? window.used_percent : null;
  const fresh = !expired && observedAt != null && now - observedAt <= CLAUDE_USAGE_FRESH_MS
    && (source === "inference_header" || !account.stale);
  return { name: window.name, used, reset, source, observedAt, expired, fresh };
}

export function claudeAccountWindowViews(account: ProviderAccountView, now: number): ClaudeWindowView[] {
  return (account.claude_usage?.windows ?? []).map((window) => claudeWindowView(window, account, now));
}

function mainViews(views: ClaudeWindowView[]): (ClaudeWindowView | undefined)[] {
  return MAIN_WINDOWS.map((name) => views.find((view) => view.name === name));
}

/** Older of the 5h/weekly observation times, mirroring the backend's `queried_at`. */
export function claudeMainObservedAt(views: ClaudeWindowView[]): number | null {
  const times = mainViews(views).map((view) => view?.observedAt ?? null);
  return times.some((time) => time == null) ? null : Math.min(...(times as number[]));
}

/** Any missing, stale or expired main window makes the whole snapshot stale. */
export function claudeMainStale(views: ClaudeWindowView[]): boolean {
  return mainViews(views).some((view) => !view?.fresh);
}

function latestObservation(account: ProviderAccountView, now: number): number {
  return Math.max(validObservedAt(account.observed_at, now) ?? 0,
    ...(account.claude_usage?.windows ?? []).map((window) => validObservedAt(window.observed_at, now) ?? 0));
}

/** Two reads of one account (local poll vs. usage response): keep the more recent read. */
export function newerClaudeAccount(
  preferred: ProviderAccountView | undefined,
  other: ProviderAccountView | undefined,
  now: number,
): ProviderAccountView | undefined {
  if (!preferred || !other) return preferred ?? other;
  return latestObservation(other, now) > latestObservation(preferred, now) ? other : preferred;
}

const pad = (value: number) => String(value).padStart(2, "0");

/** Local time: `HH:mm` on the same day as `now`, otherwise `M/D HH:mm`. */
export function shortLocalTime(epochMs: number, now: number, seconds = false): string {
  const date = new Date(epochMs);
  const today = new Date(now);
  const time = `${pad(date.getHours())}:${pad(date.getMinutes())}${seconds ? `:${pad(date.getSeconds())}` : ""}`;
  const sameDay = date.getFullYear() === today.getFullYear() && date.getMonth() === today.getMonth()
    && date.getDate() === today.getDate();
  return sameDay ? time : `${date.getMonth() + 1}/${date.getDate()} ${time}`;
}

/** Hover text: where the value came from and when it was observed. */
export function claudeWindowSourceTitle(view: ClaudeWindowView, now: number, isZh: boolean): string | undefined {
  if (!view.source) return undefined;
  const source = view.source === "inference_header"
    ? (isZh ? "来自对话响应" : "From chat responses")
    : (isZh ? "完整查询" : "Full query");
  const time = view.observedAt != null ? shortLocalTime(view.observedAt, now, true) : (isZh ? "时间未知" : "time unknown");
  return `${source} · ${time}`;
}
