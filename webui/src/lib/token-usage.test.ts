import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { compactUsageCost, formatUsageCost, groupTokenModels, groupUsageModelFamilies, sumTokenUsage, sumUsageCost, tokenCalendar, tokenDays, tokenHeat, tokenModelKey, usageBaseModel, usageCurrencies, usageCurrencyFactor } from "./token-usage";
import type { TokenUsageCost, TokenUsageDay } from "./types";

const cost = (overrides: Partial<TokenUsageCost> = {}): TokenUsageCost => ({
  currency: "USD", input_cost: 2, cache_read_cost: 1, output_cost: 3, total_cost: 6,
  cache_rate_fallback: false, pricing_provider: "openai", pricing_model: "gpt-example",
  input_rate: 2, cache_read_rate: 0.5, output_rate: 6, ...overrides,
});

const row = (overrides: Partial<TokenUsageDay> = {}): TokenUsageDay => ({
  date: "2024-02-01", provider_id: "a", provider_name: "Provider A", model: "same-model",
  request_count: 1, input_tokens: 100, output_tokens: 20, cache_read_tokens: 80,
  cache_read_reported_count: 1, missing_usage_count: 0, ...overrides,
});

test("gross input includes cache; neither daily nor model totals double count it", () => {
  const rows = [row(), row({ output_tokens: 30 })];
  equal(sumTokenUsage(rows).total_tokens, 250);
  equal(sumTokenUsage(rows).cache_read_tokens, 160);
  equal(tokenDays(rows).get("2024-02-01")?.total_tokens, 250);
  equal(groupTokenModels(rows)[0].total_tokens, 250);
});

test("identical models on different providers never merge; compound keys cannot collide", () => {
  const rows = [row(), row({ provider_id: "b", provider_name: "Provider B" }), row()];
  const models = groupTokenModels(rows);
  equal(models.length, 2);
  equal(models[0].request_count, 2);
  equal(models[0].provider_name, "Provider A");
  equal(models[1].provider_name, "Provider B");
  equal(tokenModelKey({ provider_id: "a/b", model: "c" }) === tokenModelKey({ provider_id: "a", model: "b/c" }), false);
});

test("missing usage and partial cache coverage survive grouping without inventing tokens", () => {
  const total = sumTokenUsage([row(), row({ request_count: 3, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_read_reported_count: 0, missing_usage_count: 3 })]);
  equal(total.total_tokens, 120);
  equal(total.request_count, 4);
  equal(total.cache_read_reported_count, 1);
  equal(total.missing_usage_count, 3);
  equal(sumTokenUsage([]).request_count, 0);
});

test("Monday-first UTC calendars include leap day and complete trailing weeks", () => {
  const leap = tokenCalendar(2024, 1);
  equal(leap.length, 35);
  deepEqual(leap.slice(0, 4), [null, null, null, "2024-02-01"]);
  equal(leap.filter(Boolean).length, 29);
  equal(leap[31], "2024-02-29");
  equal(tokenCalendar(2025, 1).filter(Boolean).length, 28);
  equal(tokenCalendar(2026, 2)[6], "2026-03-01");
  equal(tokenCalendar(2026, 10)[0], null);
  equal(tokenCalendar(2026, 5)[0], "2026-06-01");
});

test("USD and CNY merge at the fixed 7.0 rate for both display currencies", () => {
  const rows = [row({ cost: cost(), request_count: 2 }), row({ cost: cost({ currency: "CNY" }), request_count: 3 }), row({ cost: null }), row()];
  deepEqual(usageCurrencies(rows), ["CNY", "USD"]);
  const usd = sumUsageCost(rows, "USD");
  // USD 6 stays; CNY 6 converts at 1/7.
  equal(usd.total_cost, 6 + 6 / 7);
  equal(usd.input_cost + usd.cache_read_cost + usd.output_cost, usd.total_cost);
  equal(usd.priced_request_count, 5);
  equal(usd.other_currency_request_count, 0);
  equal(usd.unpriced_request_count, 2);
  const cny = sumUsageCost(rows, "CNY");
  equal(cny.total_cost, 6 * 7 + 6);
  equal(cny.priced_request_count, 5);
  equal(sumTokenUsage(rows, "CNY").request_count, 7);
});

test("only unsupported currencies stay excluded from merged totals", () => {
  const rows = [row({ cost: cost() }), row({ cost: cost({ currency: "EUR" }) })];
  equal(sumUsageCost(rows, "USD").other_currency_request_count, 1);
  equal(sumUsageCost(rows, "USD").priced_request_count, 1);
  equal(usageCurrencyFactor("USD", "USD"), 1);
  equal(usageCurrencyFactor("CNY", "CNY"), 1);
  equal(usageCurrencyFactor("USD", "CNY"), 7);
  equal(usageCurrencyFactor("CNY", "USD"), 1 / 7);
  equal(usageCurrencyFactor("EUR", "USD"), null);
});

test("true zero costs stay priced while null and legacy responses remain unpriced", () => {
  const zero = row({ cost: cost({ input_cost: 0, cache_read_cost: 0, output_cost: 0, total_cost: 0 }) });
  equal(sumUsageCost([zero], "USD").priced_request_count, 1);
  equal(sumUsageCost([row({ cost: null }), row()], "USD").unpriced_request_count, 2);
  equal(sumUsageCost([], "USD").priced_request_count, 0);
  deepEqual(usageCurrencies([]), []);
});

test("daily and model costs sum backend estimates and sort by selected metric", () => {
  const rows = [row({ provider_id: "cheap", input_tokens: 10000, cost: cost() }), row({ provider_id: "expensive", cost: cost({ total_cost: 20, cache_rate_fallback: true }), request_count: 3 })];
  equal(groupTokenModels(rows)[0].key, tokenModelKey(rows[0]));
  equal(groupTokenModels(rows, "USD", "cost")[0].key, tokenModelKey(rows[1]));
  equal(tokenDays(rows).get("2024-02-01")?.cost.total_cost, 26);
  equal(sumUsageCost(rows, "USD").cache_rate_fallback_count, 3);
  equal(sumUsageCost(rows, "CNY").cache_rate_fallback_count, 3);
  equal(groupTokenModels(rows, "USD", "cost")[0].pricing?.pricing_model, "gpt-example");
});

test("manual price metadata stays provider-scoped across merged currencies", () => {
  const rows = [row({ provider_id: "manual", cost: cost({ is_manual: true }), request_count: 3 }), row({ provider_id: "automatic", cost: cost() }), row({ provider_id: "cny", cost: cost({ is_manual: true, currency: "CNY" }), request_count: 2 })];
  // Both manual USD and manual CNY rows are priced after conversion.
  equal(sumUsageCost(rows, "USD").manual_request_count, 5);
  equal(sumUsageCost(rows, "CNY").manual_request_count, 5);
  const models = groupTokenModels(rows);
  equal(models.find((item) => item.provider_id === "manual")?.is_manual, true);
  equal(models.find((item) => item.provider_id === "automatic")?.is_manual, false);
  equal(models.find((item) => item.provider_id === "cny")?.is_manual, true);
  equal(models.find((item) => item.provider_id === "cny")?.pricing?.currency, "CNY");
});

test("partial usage is retained and period/day selection never changes source rows", () => {
  const rows = [row({ cost: cost(), missing_usage_count: 1 }), row({ date: "2024-03-01", cost: cost() })];
  const snapshot = JSON.stringify(rows);
  const feb = rows.filter((item) => item.date.startsWith("2024-02"));
  equal(sumTokenUsage(feb).cost.total_cost, 6);
  equal(sumTokenUsage(feb).missing_usage_count, 1);
  equal(sumTokenUsage(rows).cost.total_cost, 12);
  groupTokenModels(rows, "USD", "cost");
  equal(JSON.stringify(rows), snapshot);
});

test("currency formatting distinguishes zero, fractional value and large values", () => {
  equal(formatUsageCost(0, "USD"), "$0.00");
  equal(formatUsageCost(0.00001, "USD"), "<$0.01");
  equal(formatUsageCost(0.009, "CNY", "zh-CN", true), "<¥0.01");
  equal(formatUsageCost(1234.56, "USD"), "$1,234.56");
  equal(formatUsageCost(1250000, "USD", "en-US", true), "$1.25M");
  equal(formatUsageCost(-1, "USD"), "—");
  equal(formatUsageCost(NaN, "USD"), "—");
  equal(compactUsageCost(0.001), "<.01");
  equal(compactUsageCost(0), "0");
  equal(compactUsageCost(1234), "1K");
  equal(compactUsageCost(0.25), "0.25");
});

test("numbered Claude effort families normalize case without collapsing versions or products", () => {
  for (const family of ["sonnet", "opus", "haiku", "fable"]) {
    for (const version of ["5-5", "5-50", "4.6", "3-5-20241022"]) {
      const base = `claude-${family}-${version}`;
      for (const effort of ["max", "medium", "high", "low", "xhigh", "minimal", "none"]) {
        equal(usageBaseModel(`${base}-${effort}`), base);
        equal(usageBaseModel(` ${base}-reasoning-${effort}`.toUpperCase()), base);
      }
    }
  }
  for (const name of [
    "claude-sonnet-5-5", "claude-sonnet-5-50", "claude-opus-5-5", "claude-haiku-5-5",
    "claude-sonnet-5-5-mini", "claude-sonnet-5-5-pro", "claude-sonnet-5-5-flash",
    "claude-sonnet-5-5-pro-max", "claude-sonnet-5-5-max-preview", "claude-sonnet-max",
    "claude-sonnet-5-5-reasoning", "gpt-5-mini", "gpt-5-pro", "gemini-3-flash",
    "custom-max", "anthropic/claude-sonnet-5-5-max", "other/claude-sonnet-5-5-max",
  ]) equal(usageBaseModel(name.toUpperCase()), name);
  equal(usageBaseModel(" Unknown-Model "), "unknown-model");
});

test("Grok build-fast and Gemini effort variants collapse without losing product identities", () => {
  for (const version of ["4.7", "4.70", "4-7"]) {
    equal(usageBaseModel(` GROK-${version}-BUILD-FAST `), `grok-${version}`);
  }
  for (const product of ["flash", "pro", "flash-lite", "flash-image", "pro-preview", "flash-preview"]) {
    for (const effort of ["high", "medium", "low", "max", "xhigh", "minimal", "none", "tiered"]) {
      const base = `gemini-3.8-${product}`;
      equal(usageBaseModel(`${base}-${effort}`.toUpperCase()), base);
      equal(usageBaseModel(`${base}-reasoning-${effort}`), base);
    }
  }
  for (const model of [
    "grok-4.7-fast", "grok-4.7-mini", "grok-4.7-build", "grok-4.7-build-fast-preview",
    "grok-4.7-fast-build-fast", "grok-code-fast-1", "grok-build-fast",
    "gemini-3.8-flash", "gemini-3.8-pro", "gemini-3.80-flash", "gemini-3.8-flash-lite",
    "gemini-3.8-flash-image", "gemini-3.8-flash-high-preview", "gemini-pro-agent-high",
    "gemini-3.8-flash-unknown", "custom-build-fast", "custom-high",
    "google/gemini-3.8-flash-high", "xai/grok-4.7-build-fast",
  ]) equal(usageBaseModel(model), model);
});

test("reported Grok and Gemini pairs merge across providers with original pricing and FX intact", () => {
  for (const [base, variant] of [["grok-4.7", "grok-4.7-build-fast"], ["gemini-3.8-flash", "gemini-3.8-flash-high"]]) {
    const rows = [
      row({ model: base, cost: cost({ pricing_model: base }), request_count: 2 }),
      row({ model: variant, provider_id: "b", cost: cost({ currency: "CNY", is_manual: true, pricing_model: variant, input_cost: 14, cache_read_cost: 7, output_cost: 21, total_cost: 42 }), request_count: 3 }),
      row({ date: "2024-02-02", model: variant, cost: null, missing_usage_count: 1 }),
    ];
    const snapshot = JSON.stringify(rows);
    for (const currency of ["USD", "CNY"]) {
      const families = groupUsageModelFamilies(rows, currency, "cost");
      equal(families.length, 1);
      const [family] = families;
      equal(family.model, base);
      equal(family.variant_count, 2);
      equal(family.provider_count, 2);
      equal(family.cost.total_cost, currency === "USD" ? 12 : 84);
      equal(family.cost.unpriced_request_count, 1);
      equal(family.missing_usage_count, 1);
      deepEqual(family.variants, groupTokenModels(rows, currency, "cost"));
      deepEqual(family.cost, sumUsageCost(rows, currency));
    }
    equal(groupUsageModelFamilies([rows[1]])[0].model, base, "No base row needed to identify family");
    equal(groupUsageModelFamilies(rows.filter((item) => item.date === "2024-02-02"))[0].model, base);
    equal(JSON.stringify(rows), snapshot);
  }
});

test("base and max form one total while preserving exact names and differently priced manual variants", () => {
  const rows = [
    row({ model: "claude-sonnet-5-5", cost: cost({ pricing_model: "claude-sonnet-5-5", is_manual: true, input_rate: 2 }) }),
    row({ model: "CLAUDE-SONNET-5-5-MAX", request_count: 3, cost: cost({ pricing_model: "claude-sonnet-5-5-max", is_manual: true, input_rate: 19, total_cost: 41 }) }),
    row({ model: "claude-sonnet-5-5-max", cost: null, request_count: 2, missing_usage_count: 1 }),
  ];
  const [family] = groupUsageModelFamilies(rows);
  equal(groupUsageModelFamilies(rows).length, 1);
  equal(family.key, "claude-sonnet-5-5");
  equal(family.model, "claude-sonnet-5-5");
  equal(family.provider_count, 1);
  equal(family.variant_count, 3);
  equal(family.cost.total_cost, 47);
  equal(family.cost.priced_request_count, 4);
  equal(family.cost.manual_request_count, 4);
  equal(family.cost.unpriced_request_count, 2);
  equal(family.missing_usage_count, 1);
  deepEqual(family.variants, groupTokenModels(rows));
  equal(family.variants.find((variant) => variant.model === "CLAUDE-SONNET-5-5-MAX")?.pricing, rows[1].cost);
  equal(family.variants.find((variant) => variant.model === "claude-sonnet-5-5-max")?.pricing, null);
});

test("family identity never depends on missing, automatic, or broad manual pricing metadata", () => {
  const names = ["claude-sonnet-5-5", "claude-sonnet-5-5-max", "claude-sonnet-5-50-max", "claude-opus-5-5-max", "claude-haiku-5-5-high", "claude-sonnet-5-5-mini"];
  const prices = [undefined, null, cost({ pricing_model: "claude-sonnet-5-5" }), cost({ pricing_model: "claude", is_manual: true })];
  const expected = names.map(usageBaseModel);
  for (const price of prices) {
    const rows = names.map((model) => row({ model, cost: price }));
    const families = groupUsageModelFamilies(rows);
    equal(families.length, 5);
    for (let i = 0; i < rows.length; i++) {
      equal(groupUsageModelFamilies([rows[i]])[0].key, expected[i]);
      equal(families.find((family) => family.variants.some((variant) => variant.model === names[i]))?.key, expected[i]);
    }
  }
});

test("same model across providers merges USD/CNY estimates but keeps native variant prices", () => {
  const rows = [
    row({ model: "claude-sonnet-5-5", cost: cost(), request_count: 2 }),
    row({ model: "claude-sonnet-5-5", provider_id: "b", provider_name: "Provider B", cost: cost({ currency: "CNY", input_cost: 14, cache_read_cost: 7, output_cost: 21, total_cost: 42 }), request_count: 3 }),
  ];
  for (const currency of ["USD", "CNY"]) {
    const [family] = groupUsageModelFamilies(rows, currency, "cost");
    equal(family.provider_count, 2);
    equal(family.variant_count, 1);
    equal(family.variants.length, 2);
    equal(family.cost.total_cost, currency === "USD" ? 12 : 84);
    equal(family.cost.priced_request_count, 5);
    equal(family.cost.other_currency_request_count, 0);
    deepEqual(family.variants, groupTokenModels(rows, currency, "cost"));
    equal(family.variants.find((variant) => variant.provider_id === "b")?.pricing?.currency, "CNY");
  }
});

test("family totals equal children and global totals including all coverage counters", () => {
  const rows = [
    row({ model: "claude-sonnet-5-5", cost: cost({ is_manual: true, cache_rate_fallback: true }), request_count: 3 }),
    row({ model: "claude-sonnet-5-5-max", provider_id: "b", cost: cost({ currency: "CNY" }), missing_usage_count: 1 }),
    row({ model: "claude-sonnet-5-5-high", cost: null, request_count: 2 }),
    row({ model: "claude-sonnet-5-5-low", cost: cost({ currency: "EUR" }), request_count: 4 }),
    row({ model: "claude-opus-5-5", cost: cost() }),
    row({ model: "claude-opus-5-5-max", input_tokens: 0, output_tokens: 0, missing_usage_count: 1 }),
  ];
  type Totals = ReturnType<typeof sumTokenUsage>;
  const assertSum = (parent: Totals, children: Totals[]) => {
    for (const key of ["request_count", "input_tokens", "output_tokens", "cache_read_tokens", "cache_read_reported_count", "missing_usage_count", "total_tokens"] as const) {
      equal(children.reduce((sum, child) => sum + child[key], 0), parent[key], key);
    }
    for (const key of ["input_cost", "output_cost", "cache_read_cost", "total_cost", "priced_request_count", "unpriced_request_count", "other_currency_request_count", "cache_rate_fallback_count", "manual_request_count"] as const) {
      const sum = children.reduce((total, child) => total + child.cost[key], 0);
      equal(Math.abs(sum - parent.cost[key]) < 1e-10, true, key);
    }
  };
  for (const currency of ["USD", "CNY"]) {
    const families = groupUsageModelFamilies(rows, currency);
    for (const family of families) assertSum(family, family.variants);
    assertSum(sumTokenUsage(rows, currency), families);
  }
});

test("family and child ordering follow the chosen metric with stable key tie breaks", () => {
  const rows = [
    row({ model: "claude-sonnet-5-5", input_tokens: 1000, cost: cost() }),
    row({ model: "claude-sonnet-5-5-max", input_tokens: 10, cost: cost({ total_cost: 20 }) }),
    row({ model: "claude-opus-5-5", cost: cost({ total_cost: 40 }) }),
    row({ model: "custom-b", cost: cost({ total_cost: 40 }) }),
  ];
  const tokens = groupUsageModelFamilies(rows);
  equal(tokens[0].key, "claude-sonnet-5-5");
  equal(tokens[0].variants[0].model, "claude-sonnet-5-5");
  const costs = groupUsageModelFamilies(rows, "USD", "cost");
  deepEqual(costs.map((family) => family.key), ["claude-opus-5-5", "custom-b", "claude-sonnet-5-5"]);
  equal(costs[2].variants[0].model, "claude-sonnet-5-5-max");
  deepEqual(groupUsageModelFamilies([...rows].reverse(), "USD", "cost").map((family) => family.key), costs.map((family) => family.key));
  deepEqual(groupUsageModelFamilies([]), []);
});

test("day and period subsets keep stable families without mutating source rows or prices", () => {
  const rows = [
    row({ model: "claude-sonnet-5-5", cost: cost() }),
    row({ date: "2024-02-02", model: "claude-sonnet-5-5-max", cost: null }),
    row({ date: "2024-03-01", model: "claude-sonnet-5-5-reasoning-high", provider_id: "b", cost: cost({ is_manual: true, pricing_model: "claude-sonnet", total_cost: 30 }) }),
  ];
  const snapshot = JSON.stringify(rows);
  for (const item of rows) {
    if (item.cost) Object.freeze(item.cost);
    Object.freeze(item);
  }
  Object.freeze(rows);
  const all = groupUsageModelFamilies(rows, "USD", "cost")[0];
  equal(all.cost.total_cost, 36);
  for (const date of ["2024-02-01", "2024-02-02", "2024-03-01"]) {
    const day = groupUsageModelFamilies(rows.filter((item) => item.date === date))[0];
    equal(day.key, all.key);
    deepEqual(day.cost, tokenDays(rows).get(date)?.cost);
  }
  const feb = groupUsageModelFamilies(rows.filter((item) => item.date.startsWith("2024-02")))[0];
  equal(feb.key, all.key);
  equal(feb.cost.total_cost, 6);
  equal(feb.cost.unpriced_request_count, 1);
  equal(feb.provider_count, 1);
  equal(feb.variant_count, 2);
  equal(JSON.stringify(rows), snapshot);
});

test("heat scale distinguishes empty days and stays within the legend", () => {
  equal(tokenHeat(0, 0), 0);
  equal(tokenHeat(0, 100), 0);
  equal(tokenHeat(1, 100), 1);
  equal(tokenHeat(100, 100), 5);
  equal(tokenHeat(200, 100), 5);
});
