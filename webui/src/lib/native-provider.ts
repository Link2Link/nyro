import type { ProviderProtocol } from "./types";

export const DEVIN_PROTOCOL = "devin-chat/get-chat-message/v1";
export const KIRO_PROTOCOL = "kiro-chat/generate-assistant-response/v1";

type ProviderIdentity = { vendor?: string | null; preset_key?: string | null };

export function isDevinProvider(provider?: ProviderIdentity | null): boolean {
  return [provider?.vendor, provider?.preset_key].some((value) => value?.trim().toLowerCase() === "devin");
}

export function isKiroProvider(provider?: ProviderIdentity | null): boolean {
  return [provider?.vendor, provider?.preset_key].some((value) => value?.trim().toLowerCase() === "kiro");
}

/** Preserve native transport identity even when an imported row has no preset. */
export function nativeProviderProtocol(provider?: ProviderIdentity | null): ProviderProtocol | null {
  if (isDevinProvider(provider)) return DEVIN_PROTOCOL;
  if (isKiroProvider(provider)) return KIRO_PROTOCOL;
  return null;
}

/** Native catalogs are supplied by the gateway, not a generic HTTP models URL. */
export function hasProviderModelCatalog(provider?: (ProviderIdentity & { models_source?: string | null }) | null): boolean {
  return Boolean(provider?.models_source?.trim()) || nativeProviderProtocol(provider) !== null;
}

/** Native OAuth transports never enter the generic API-key/adaptive protocol tables. */
export function nativePresetProtocol(preset?: { id: string; defaultProtocol: string } | null): ProviderProtocol | null {
  if (preset?.id === "devin" && ["devin-chat", DEVIN_PROTOCOL].includes(preset.defaultProtocol)) {
    return DEVIN_PROTOCOL;
  }
  if (preset?.id === "kiro" && ["kiro-chat", KIRO_PROTOCOL].includes(preset.defaultProtocol)) {
    return KIRO_PROTOCOL;
  }
  return null;
}
