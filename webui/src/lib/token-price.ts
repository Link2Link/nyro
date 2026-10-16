import type { TokenPriceConfig, TokenPriceRates } from "./types";

export const MAX_TOKEN_PRICE = 1_000_000_000;
export interface TokenPriceDraft {
  currency: "USD" | "CNY";
  input: string;
  cache: string;
  output: string;
}
export type TokenPriceField = "input" | "cache" | "output";

export function tokenPriceDraft(config: TokenPriceConfig): TokenPriceDraft {
  const rates = config.override_price ?? config.inherited_price ?? config.default_price;
  return {
    currency: rates?.currency ?? "USD",
    input: rates ? String(rates.input_rate) : "",
    output: rates ? String(rates.output_rate) : "",
    cache: rates?.cache_read_rate == null ? "" : String(rates.cache_read_rate),
  };
}

/** Blank cache means input-rate fallback; blank mandatory fields never become zero. */
export function parseTokenPriceDraft(draft: TokenPriceDraft): { price: TokenPriceRates; invalid: null } | { price: null; invalid: TokenPriceField } {
  const parse = (text: string) => {
    if (!text.trim()) return null;
    // Decimal/scientific notation only: reject JS hex, Infinity and locale separators.
    if (!/^(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?$/i.test(text.trim())) return null;
    const value = Number(text);
    return Number.isFinite(value) && value >= 0 && value <= MAX_TOKEN_PRICE ? value : null;
  };
  const input = parse(draft.input);
  if (input === null) return { price: null, invalid: "input" };
  const output = parse(draft.output);
  if (output === null) return { price: null, invalid: "output" };
  const cache = draft.cache.trim() ? parse(draft.cache) : null;
  if (draft.cache.trim() && cache === null) return { price: null, invalid: "cache" };
  return { price: { currency: draft.currency, input_rate: input, output_rate: output, cache_read_rate: cache }, invalid: null };
}
