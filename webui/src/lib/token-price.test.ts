import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { MAX_TOKEN_PRICE, parseTokenPriceDraft, tokenPriceDraft, type TokenPriceDraft } from "./token-price";
import type { TokenPriceConfig } from "./types";

const config: TokenPriceConfig = {
  provider_id: "one", model: "gpt-example", override_price: null,
  default_price: { currency: "USD", input_rate: 2, cache_read_rate: 0.5, output_rate: 6, pricing_provider: "openai", pricing_model: "gpt-example" },
};
const draft = (overrides: Partial<TokenPriceDraft> = {}): TokenPriceDraft => ({ currency: "USD", input: "2", cache: "0.5", output: "6", ...overrides });

test("defaults prefill form without creating a manual override", () => {
  deepEqual(tokenPriceDraft(config), draft());
  equal(config.override_price, null);
  deepEqual(tokenPriceDraft({ ...config, default_price: null }), draft({ input: "", cache: "", output: "" }));
});

test("saved override takes precedence and preserves zero and null cache", () => {
  const overridden: TokenPriceConfig = { ...config, override_price: { currency: "CNY", input_rate: 0, cache_read_rate: null, output_rate: 8.25 } };
  deepEqual(tokenPriceDraft(overridden), draft({ currency: "CNY", input: "0", cache: "", output: "8.25" }));
  deepEqual(parseTokenPriceDraft(tokenPriceDraft(overridden)).price, overridden.override_price);
  equal(tokenPriceDraft({ ...config, default_price: { ...config.default_price!, cache_read_rate: null } }).cache, "");
});

test("blank cache is fallback but explicit zero remains free", () => {
  equal(parseTokenPriceDraft(draft({ cache: " " })).price?.cache_read_rate, null);
  equal(parseTokenPriceDraft(draft({ cache: "0" })).price?.cache_read_rate, 0);
  equal(parseTokenPriceDraft(draft({ input: "0", output: "0" })).price?.input_rate, 0);
  equal(parseTokenPriceDraft(draft({ input: "" })).invalid, "input");
  equal(parseTokenPriceDraft(draft({ output: " " })).invalid, "output");
});

test("invalid, negative, non-finite and oversized rates are rejected for each field", () => {
  for (const value of ["-1", "NaN", "Infinity", "1e309", String(MAX_TOKEN_PRICE + 1), "0x10", "1,23", "abc", "1_000", "1e"]) {
    for (const field of ["input", "cache", "output"] as const) equal(parseTokenPriceDraft(draft({ [field]: value })).invalid, field, `${field}: ${value}`);
  }
  equal(parseTokenPriceDraft(draft({ input: String(MAX_TOKEN_PRICE) })).invalid, null);
  equal(parseTokenPriceDraft(draft({ input: " 0.00001 ", output: "1e-7" })).price?.output_rate, 1e-7);
  equal(parseTokenPriceDraft(draft({ cache: ".25" })).price?.cache_read_rate, 0.25);
});

test("changing currency never silently converts user-entered rates", () => {
  deepEqual(parseTokenPriceDraft(draft({ currency: "CNY" })).price, { currency: "CNY", input_rate: 2, cache_read_rate: 0.5, output_rate: 6 });
});

test("inherited prefix rates prefill when the model has no own override", () => {
  const inherited: TokenPriceConfig = { ...config, inherited_price: { currency: "CNY", input_rate: 8, output_rate: 28, cache_read_rate: null, pricing_provider: "manual", pricing_model: "claude-haiku-5-5" } };
  deepEqual(tokenPriceDraft(inherited), draft({ currency: "CNY", input: "8", output: "28", cache: "" }));
  deepEqual(parseTokenPriceDraft(tokenPriceDraft(inherited)).price, { currency: "CNY", input_rate: 8, output_rate: 28, cache_read_rate: null });
  const overridden: TokenPriceConfig = { ...inherited, override_price: { currency: "USD", input_rate: 1, output_rate: 2, cache_read_rate: 3 } };
  deepEqual(tokenPriceDraft(overridden), draft({ input: "1", output: "2", cache: "3" }));
});
