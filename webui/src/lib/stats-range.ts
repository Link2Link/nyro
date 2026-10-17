/**
 * Time range model for the request-analytics views.
 *
 * Presets are rolling "last N hours" windows. "month" is month-to-date in the
 * browser's local time zone; "custom" is an explicit start with an optional end
 * (a missing end means "until now", so the view keeps updating).
 */
export const PRESET_HOURS = [6, 24, 72, 168, 720] as const;
/** The backend rejects custom windows longer than this. */
export const MAX_CUSTOM_DAYS = 366;

export type StatsRange =
  | { kind: "hours"; hours: number }
  | { kind: "month" }
  | { kind: "custom"; startAt: number; endAt: number | null };

export type StatsRangeError = "start_required" | "end_invalid" | "start_future" | "order" | "too_long";

export type StatsRangeArgs = {
  hours?: number;
  startAt?: number;
  endAt?: number;
};

export const DEFAULT_RANGE: StatsRange = { kind: "hours", hours: 24 };

const HOUR_MS = 3_600_000;
const DAY_MS = 24 * HOUR_MS;

/** 00:00 on the first day of `now`'s month, local time (epoch ms). */
export function monthStart(now: number = Date.now()): number {
  const date = new Date(now);
  return new Date(date.getFullYear(), date.getMonth(), 1).getTime();
}

/** Start of the window as epoch ms. */
export function rangeStart(range: StatsRange, now: number = Date.now()): number {
  if (range.kind === "hours") return now - range.hours * HOUR_MS;
  if (range.kind === "month") return monthStart(now);
  return range.startAt;
}

/** Arguments for the stats backend calls. Month and custom ranges send bounds, not hours. */
export function rangeArgs(range: StatsRange, now: number = Date.now()): StatsRangeArgs {
  if (range.kind === "hours") return { hours: range.hours };
  if (range.kind === "month") return { startAt: monthStart(now) };
  return range.endAt == null ? { startAt: range.startAt } : { startAt: range.startAt, endAt: range.endAt };
}

/** Stable react-query key fragment; a new month yields a new key. */
export function rangeKey(range: StatsRange, now: number = Date.now()): string {
  if (range.kind === "hours") return `hours:${range.hours}`;
  if (range.kind === "month") return `month:${monthStart(now)}`;
  return `custom:${range.startAt}:${range.endAt ?? ""}`;
}

/** Window length in hours, used to decide whether chart axes need dates. */
export function rangeSpanHours(range: StatsRange, now: number = Date.now()): number {
  if (range.kind === "hours") return range.hours;
  const end = range.kind === "custom" ? (range.endAt ?? now) : now;
  return Math.max(0, (end - rangeStart(range, now)) / HOUR_MS);
}

/** Mirror of the server-side checks so the user gets feedback before a request. */
export function validateCustomRange(
  startAt: number | null,
  endAt: number | null,
  now: number = Date.now(),
): StatsRangeError | null {
  if (startAt === null || !Number.isFinite(startAt) || startAt < 0) return "start_required";
  if (startAt >= now) return "start_future";
  const end = endAt === null ? now : Math.min(endAt, now);
  if (endAt !== null && !Number.isFinite(endAt)) return "end_invalid";
  if (startAt >= end) return "order";
  if (end - startAt > MAX_CUSTOM_DAYS * DAY_MS) return "too_long";
  return null;
}

const pad = (value: number) => String(value).padStart(2, "0");

/** Epoch ms to the `YYYY-MM-DDTHH:mm` string a datetime-local input expects (local time). */
export function toDatetimeLocal(ms: number): string {
  const date = new Date(ms);
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** Parse a datetime-local value as local time; null when empty or invalid. */
export function fromDatetimeLocal(value: string): number | null {
  if (!value.trim()) return null;
  const ms = new Date(value).getTime();
  return Number.isNaN(ms) ? null : ms;
}
