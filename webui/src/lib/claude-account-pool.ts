import type { Provider } from "./types";

/** Never infer OAuth from the Anthropic hostname or enable pools for API keys. */
export function isClaudeCodeOAuthProvider(provider?: Pick<Provider, "vendor" | "preset_key" | "channel" | "auth_mode"> | null): boolean {
  const identity = (provider?.vendor || provider?.preset_key)?.trim().toLowerCase();
  return identity === "anthropic" && provider?.channel?.trim().toLowerCase() === "claude-code" && provider?.auth_mode === "oauth";
}
