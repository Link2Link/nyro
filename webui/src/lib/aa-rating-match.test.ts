import test from "node:test";
import assert from "node:assert/strict";
import { buildAaLookupIndex, matchAaModel, normalizeModelKey, resolveEffectiveRating } from "./aa-rating-match";
import type { AaModelAlias, AaModelRatingEntry, ModelRatingEntry } from "./types";

function aa(canonical_key: string, aa_name: string, raw_score: number, effort = "max"): AaModelRatingEntry {
  return {
    canonical_key,
    aa_slug: canonical_key,
    aa_name,
    aa_creator: "Test",
    selected_effort: effort,
    raw_score,
    score: Math.round(raw_score),
  };
}

test("normalizeModelKey aligns hyphen, dot, space, namespace, and Claude order variants", () => {
  for (const input of [
    "claude-opus-5-5",
    "claude-opus-5.5",
    "Claude-Opus-5.5",
    "Claude Opus 5.5",
    "claude_opus_5_5",
    "anthropic/claude-opus-5.5",
    "Claude Opus 5.5 (Max, Default Fallback)",
    "anthropic/claude-opus-5-5:free",
  ]) {
    assert.equal(normalizeModelKey(input), "claude-opus-5-5", input);
  }
  assert.equal(normalizeModelKey("claude-4-5-sonnet"), "claude-sonnet-4-5");
  assert.equal(normalizeModelKey("Claude 4.5 Sonnet"), "claude-sonnet-4-5");
  assert.equal(normalizeModelKey("claude-5-fable"), "claude-fable-5");
  assert.equal(normalizeModelKey("Qwen3.5 397B A17B"), "qwen-3-5-397b-a17b");
  assert.equal(normalizeModelKey("GPT-6.2 Astra"), "gpt-6-2-astra");
});

test("matchAaModel matches VPS variants and rejects distinct product tiers", () => {
  const index = buildAaLookupIndex([
    aa("claude-opus-5-5", "Claude Opus 5.5 (Max)", 59.1),
    aa("claude-fable-5-2", "Claude Fable 5.2 (Max)", 65.0),
    aa("claude-sonnet-4-5", "Claude 4.5 Sonnet (Reasoning)", 48.0, "reasoning"),
    aa("gpt-6-2-astra", "GPT-6.2 Astra (Max)", 62.0),
    aa("gemini-4-neon", "Gemini 4 Neon (High)", 61.0, "high"),
    aa("gemini-3-8-flash", "Gemini 3.8 Flash (High)", 31.0, "high"),
    aa("gemini-3-1-pro-preview", "Gemini 3.1 Pro Preview (High)", 52.0, "high"),
    aa("grok-4-7", "Grok 4.7 (Xhigh)", 35.0, "xhigh"),
    aa("mistral-medium", "Mistral Medium", 33.0, "default"),
  ]);

  assert.equal(matchAaModel("Claude Opus 5.5", index)?.score, 59);
  assert.equal(matchAaModel("claude-opus-5.5", index)?.score, 59);
  assert.equal(matchAaModel("claude-opus-5-5-xhigh", index)?.score, 59);
  assert.equal(matchAaModel("anthropic/claude-sonnet-4.5", index)?.score, 48);
  assert.equal(matchAaModel("claude-sonnet-4-5-20250929", index)?.score, 48);
  assert.equal(matchAaModel("gpt-6.2-astra", index)?.score, 62);
  assert.equal(matchAaModel("gemini-4-neon-xhigh", index)?.score, 61);
  assert.equal(matchAaModel("gemini-3.8-flash-tiered", index)?.score, 31);
  assert.equal(matchAaModel("gemini-3-1-pro-high", index)?.score, 52);
  assert.equal(matchAaModel("grok-4.7-build-fast", index)?.score, 35);
  assert.equal(matchAaModel("mistral-medium", index)?.score, 33);

  for (const unmatched of [
    "codex-auto-review",
    "gpt-6-2-mini",
    "gemini-3.8-flash-lite",
    "claude-opus-5-55",
    "claude-opus-6",
  ]) {
    assert.equal(matchAaModel(unmatched, index), null, unmatched);
  }
});

test("resolveEffectiveRating enforces manual > aa_alias > aa_auto priority", () => {
  const index = buildAaLookupIndex([
    aa("claude-opus-5-5", "Claude Opus 5.5 (Max)", 59.1),
    aa("gpt-6-2-astra", "GPT-6.2 Astra (Max)", 62.0),
  ]);
  const manual: ModelRatingEntry[] = [
    { model_prefix: "claude-opus-5-5", score: 80, updated_at: "2026-11-20T00:00:00.000Z" },
  ];
  const aliases: AaModelAlias[] = [
    { model_prefix: "my-astra", aa_canonical_key: "gpt-6-2-astra", updated_at: "2026-11-20T00:00:00.000Z" },
  ];

  const r1 = resolveEffectiveRating("Claude Opus 5.5", manual, aliases, index, "2026-11-23T00:00:00.000Z");
  assert.equal(r1?.source, "manual");
  assert.equal(r1?.score, 80);

  const r2 = resolveEffectiveRating("my-astra-fast", manual, aliases, index, "2026-11-23T00:00:00.000Z");
  assert.equal(r2?.source, "aa_alias");
  assert.equal(r2?.score, 62);

  const r3 = resolveEffectiveRating("GPT-6.2 Astra", manual, aliases, index, "2026-11-23T00:00:00.000Z");
  assert.equal(r3?.source, "aa_auto");
  assert.equal(r3?.score, 62);
});
