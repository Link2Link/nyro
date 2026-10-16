import type { TokenUsageDay } from "./types";

export type TokenUsageTotals = Pick<TokenUsageDay,
  "request_count" | "input_tokens" | "output_tokens" | "cache_read_tokens" |
  "cache_read_reported_count" | "missing_usage_count"> & { total_tokens: number };

export interface UsageCostTotals {
  currency: string;
  input_cost: number;
  cache_read_cost: number;
  output_cost: number;
  total_cost: number;
  priced_request_count: number;
  unpriced_request_count: number;
  other_currency_request_count: number;
  cache_rate_fallback_count: number;
  manual_request_count: number;
}

/** Fixed user-selected display conversion. Native stored rates are never rewritten. */
export const USD_CNY_RATE = 7.0;
export const USAGE_DISPLAY_CURRENCIES = ["USD", "CNY"] as const;

export function usageCurrencyFactor(from: string, to: string): number | null {
  if (from === to) return 1;
  if (from === "USD" && to === "CNY") return USD_CNY_RATE;
  if (from === "CNY" && to === "USD") return 1 / USD_CNY_RATE;
  return null;
}

/** Merge native estimates after conversion, without rounding individual rows. */
export function sumUsageCost(rows: TokenUsageDay[], currency: string): UsageCostTotals {
  const total: UsageCostTotals = { currency, input_cost: 0, cache_read_cost: 0, output_cost: 0, total_cost: 0,
    priced_request_count: 0, unpriced_request_count: 0, other_currency_request_count: 0, cache_rate_fallback_count: 0, manual_request_count: 0 };
  for (const row of rows) {
    const factor = row.cost ? usageCurrencyFactor(row.cost.currency, currency) : null;
    if (!row.cost) {
      total.unpriced_request_count += row.request_count;
    } else if (factor === null) {
      total.other_currency_request_count += row.request_count;
    } else {
      total.input_cost += row.cost.input_cost * factor;
      total.cache_read_cost += row.cost.cache_read_cost * factor;
      total.output_cost += row.cost.output_cost * factor;
      total.total_cost += row.cost.total_cost * factor;
      total.priced_request_count += row.request_count;
      if (row.cost.cache_rate_fallback) total.cache_rate_fallback_count += row.request_count;
      if (row.cost.is_manual) total.manual_request_count += row.request_count;
    }
  }
  return total;
}

export function usageCurrencies(rows: TokenUsageDay[]): string[] {
  return [...new Set(rows.flatMap((row) => row.cost ? [row.cost.currency] : []))].sort();
}

/** Calendar cells on narrow screens use the currency already shown in the heading. */
export function compactUsageCost(value: number): string {
  if (value > 0 && value < 0.01) return "<.01";
  return new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: value < 1 ? 2 : 0 }).format(value);
}

/** Preserve small nonzero estimates instead of presenting them as free. */
export function formatUsageCost(value: number, currency: string, locale = "en-US", compact = false): string {
  const symbol = currency === "USD" ? "$" : currency === "CNY" ? "¥" : `${currency} `;
  if (!Number.isFinite(value) || value < 0) return "—";
  if (value > 0 && value < 0.01) return `<${symbol}0.01`;
  return symbol + new Intl.NumberFormat(locale, {
    notation: compact ? "compact" : "standard",
    minimumFractionDigits: compact ? 0 : 2,
    maximumFractionDigits: 2,
  }).format(value);
}

export function sumTokenUsage(rows: TokenUsageDay[], currency = "USD"): TokenUsageTotals & { cost: UsageCostTotals } {
  return { ...sumTokens(rows), cost: sumUsageCost(rows, currency) };
}

function sumTokens(rows: TokenUsageDay[]): TokenUsageTotals {
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

export function groupTokenModels(rows: TokenUsageDay[], currency = "USD", metric: "tokens" | "cost" = "tokens") {
  const groups = new Map<string, TokenUsageDay[]>();
  for (const row of rows) {
    const key = tokenModelKey(row);
    const group = groups.get(key) ?? [];
    group.push(row);
    groups.set(key, group);
  }
  return [...groups].map(([key, group]) => ({
    key, model: group[0].model, provider_id: group[0].provider_id, provider_name: group[0].provider_name,
    is_manual: group.some((row) => row.cost?.is_manual),
    // Reference table and editor retain native rates; summary values are converted.
    pricing: group.find((row) => row.cost)?.cost ?? null,
    ...sumTokenUsage(group, currency),
  })).sort((a, b) => (metric === "cost" ? b.cost.total_cost - a.cost.total_cost : b.total_tokens - a.total_tokens) || a.key.localeCompare(b.key));
}

/**
 * Display identity only: numbered Claude families have known effort variants.
 * Keep versions, product suffixes and namespaces intact; never infer identity from
 * mutable pricing_model metadata or from whichever rows happen to be visible.
 */
export function usageBaseModel(model: string): string {
  const normalized = model.trim().toLowerCase();
  return normalized.replace(
    /^(claude-(?:sonnet|opus|haiku|fable)-\d+(?:[.-]\d+)*)(?:-reasoning)?-(?:max|medium|high|low|xhigh|minimal|none)$/,
    "$1",
  );
}

/** Cross-provider display families; exact variants still own prices and edit actions. */
export function groupUsageModelFamilies(rows: TokenUsageDay[], currency = "USD", metric: "tokens" | "cost" = "tokens") {
  const groups = new Map<string, TokenUsageDay[]>();
  for (const row of rows) {
    const key = usageBaseModel(row.model);
    const group = groups.get(key) ?? [];
    group.push(row);
    groups.set(key, group);
  }
  return [...groups].map(([key, group]) => {
    const variants = groupTokenModels(group, currency, metric);
    return {
      key, model: key, variants,
      provider_count: new Set(group.map((row) => row.provider_id)).size,
      // A model served by two providers is one named variant, but two edit targets.
      variant_count: new Set(group.map((row) => row.model)).size,
      // Sum existing estimates, including FX/coverage metadata; never reprice a family.
      ...sumTokenUsage(group, currency),
    };
  }).sort((a, b) => (metric === "cost" ? b.cost.total_cost - a.cost.total_cost : b.total_tokens - a.total_tokens) || a.key.localeCompare(b.key));
}

export function tokenDays(rows: TokenUsageDay[], currency = "USD") {
  const days = new Map<string, TokenUsageDay[]>();
  for (const row of rows) {
    const day = days.get(row.date) ?? [];
    day.push(row);
    days.set(row.date, day);
  }
  return new Map([...days].map(([date, group]) => [date, sumTokenUsage(group, currency)]));
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
