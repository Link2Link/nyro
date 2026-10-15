import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { groupTokenModels, sumTokenUsage, tokenCalendar, tokenDays, tokenHeat, tokenModelKey } from "./token-usage";
import type { TokenUsageDay } from "./types";

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

test("heat scale distinguishes empty days and stays within the legend", () => {
  equal(tokenHeat(0, 0), 0);
  equal(tokenHeat(0, 100), 0);
  equal(tokenHeat(1, 100), 1);
  equal(tokenHeat(100, 100), 5);
  equal(tokenHeat(200, 100), 5);
});
