import type { OpenAiSpeedMode } from "./types";

export function supportsSpeedMode(channel?: string | null): boolean {
  return channel === "codex" || channel === "sub2api";
}

/** Never carry a hidden tier to another channel. Accept booleans from older providers. */
export function speedModeForChannel(channel: string | null | undefined, mode: unknown): OpenAiSpeedMode {
  if (!supportsSpeedMode(channel)) return "normal";
  if (mode === "ultrafast") return "ultrafast";
  return mode === "fast" || mode === true ? "fast" : "normal";
}
