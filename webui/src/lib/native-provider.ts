import type { ProviderProtocol } from "./types";

export const DEVIN_PROTOCOL = "devin-chat/get-chat-message/v1";

type ProviderIdentity = { vendor?: string | null; preset_key?: string | null };

export function isDevinProvider(provider?: ProviderIdentity | null): boolean {
  return [provider?.vendor, provider?.preset_key].some((value) => value?.trim().toLowerCase() === "devin");
}

/** Native OAuth transports never enter the generic API-key/adaptive protocol tables. */
export function nativePresetProtocol(preset?: { id: string; defaultProtocol: string } | null): ProviderProtocol | null {
  if (preset?.id === "cursor" && preset.defaultProtocol === "cursor-agent/run/v1") {
    return "cursor-agent/run/v1";
  }
  if (preset?.id === "devin" && ["devin-chat", DEVIN_PROTOCOL].includes(preset.defaultProtocol)) {
    return DEVIN_PROTOCOL;
  }
  return null;
}
