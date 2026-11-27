import { isSub2ApiPreset } from "./sub2api";
import type { OpenAiSpeedMode } from "./types";

/**
 * Codex consumer upstreams carry the Responses speed tier: the codex OAuth channel, the
 * legacy `sub2api` channel and the Sub2API preset's OpenAI platform. Mirrors nyro-core
 * `provider::dialect::is_codex_consumer`.
 */
export function supportsSpeedMode(channel?: string | null, presetId?: string | null): boolean {
  const normalized = channel?.trim().toLowerCase();
  if (normalized === "codex" || normalized === "sub2api") return true;
  return normalized === "openai" && isSub2ApiPreset(presetId);
}

/** Never carry a hidden tier to another channel. Accept booleans from older providers. */
export function speedModeForChannel(
  channel: string | null | undefined,
  mode: unknown,
  presetId?: string | null,
): OpenAiSpeedMode {
  if (!supportsSpeedMode(channel, presetId)) return "normal";
  if (mode === "ultrafast") return "ultrafast";
  return mode === "fast" || mode === true ? "fast" : "normal";
}
