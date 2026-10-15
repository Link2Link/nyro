import type { TokenUsageDay } from "./types";

export type TokenUsageTotals = Pick<TokenUsageDay,
  "request_count" | "input_tokens" | "output_tokens" | "cache_read_tokens" |
  "cache_read_reported_count" | "missing_usage_count"> & { total_tokens: number };

export function sumTokenUsage(rows: TokenUsageDay[]): TokenUsageTotals {
  return rows.reduce<TokenUsageTotals>((total, row) => ({
    request_count: total.request_count + row.request_count,
    input_tokens: total.input_tokens + row.input_tokens,
    output_tokens: total.output_tokens + row.output_tokens,
    cache_read_tokens: total.cache_read_tokens + row.cache_read_tokens,
    cache_read_reported_count: total.cache_read_reported_count + row.cache_read_reported_count,
    missing_usage_count: total.missing_usage_count + row.missing_usage_count,
    // Nyro input is GROSS: cache is a subset, never add it again.
    total_tokens: total.total_tokens + row.input_tokens + row.output_tokens,
  }), { request_count: 0, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0,
    cache_read_reported_count: 0, missing_usage_count: 0, total_tokens: 0 });
}

export const tokenModelKey = (row: Pick<TokenUsageDay, "provider_id" | "model">) =>
  JSON.stringify([row.provider_id, row.model]);

export function groupTokenModels(rows: TokenUsageDay[]) {
  const groups = new Map<string, TokenUsageDay[]>();
  for (const row of rows) {
    const key = tokenModelKey(row);
    const group = groups.get(key) ?? [];
    group.push(row);
    groups.set(key, group);
  }
  return [...groups].map(([key, group]) => ({
    key, model: group[0].model, provider_name: group[0].provider_name,
    ...sumTokenUsage(group),
  })).sort((a, b) => b.total_tokens - a.total_tokens || a.key.localeCompare(b.key));
}

export function tokenDays(rows: TokenUsageDay[]) {
  const days = new Map<string, TokenUsageDay[]>();
  for (const row of rows) {
    const day = days.get(row.date) ?? [];
    day.push(row);
    days.set(row.date, day);
  }
  return new Map([...days].map(([date, group]) => [date, sumTokenUsage(group)]));
}

/** Monday-first calendar, using UTC just like the backend daily buckets. */
export function tokenCalendar(year: number, month: number): (string | null)[] {
  const first = new Date(Date.UTC(year, month, 1));
  const length = new Date(Date.UTC(year, month + 1, 0)).getUTCDate();
  const leading = (first.getUTCDay() + 6) % 7;
  const dates: (string | null)[] = Array.from({ length: leading }, () => null);
  for (let day = 1; day <= length; day++) {
    dates.push(`${year}-${String(month + 1).padStart(2, "0")}-${String(day).padStart(2, "0")}`);
  }
  while (dates.length % 7 !== 0) dates.push(null);
  return dates;
}

export function tokenHeat(total: number, peak: number): number {
  return total > 0 && peak > 0 ? Math.min(5, Math.max(1, Math.ceil(Math.sqrt(total / peak) * 5))) : 0;
}
