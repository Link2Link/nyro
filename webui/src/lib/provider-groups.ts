import type { Provider, ProviderPreset, TestResult } from "./types";

export const PROVIDER_GROUP_STORAGE_KEY = "nyro.provider-groups.v1";

/** Identity only: protocols, icons and display-name heuristics are not brands. */
export function providerGroupKey(provider: Pick<Provider, "preset_key" | "vendor">): string {
  for (const value of [provider.preset_key, provider.vendor]) {
    const key = value?.trim().toLowerCase();
    if (key && key !== "custom") return key;
  }
  return "custom";
}

export interface ProviderGroup {
  key: string;
  label: string;
  providers: Provider[];
  matches: Provider[];
}

export function groupProviders(
  providers: Provider[],
  presets: ProviderPreset[],
  isZh: boolean,
  search = "",
): ProviderGroup[] {
  const groups = new Map<string, ProviderGroup>();
  const query = search.trim().toLowerCase();
  for (const provider of providers) {
    const key = providerGroupKey(provider);
    let group = groups.get(key);
    if (!group) {
      const preset = presets.find((item) => item.id.trim().toLowerCase() === key);
      const label = key === "custom"
        ? (isZh ? "自定义" : "Custom")
        : (isZh ? preset?.label.zh : preset?.label.en) || preset?.label.en || provider.vendor?.trim() || key;
      group = { key, label, providers: [], matches: [] };
      groups.set(key, group);
    }
    group.providers.push(provider);
    const searchable = [group.label, key, provider.name, provider.vendor, provider.channel,
      provider.protocol, provider.auth_mode, ...(provider.protocol_endpoints ?? []).map((endpoint) => endpoint.protocol)];
    if (!query || searchable.some((value) => value?.toLowerCase().includes(query))) {
      group.matches.push(provider);
    }
  }
  // Keep backend order: live status updates must not move rows around.
  return [...groups.values()];
}

export function providerNeedsAttention(provider: Provider, result?: TestResult): boolean {
  return (result ? !result.success : provider.last_test_success === false)
    || (provider.auth_mode === "oauth" && Boolean(provider.oauth_status)
      && ["not_connected", "unavailable", "quota_exhausted", "error", "disconnected"].includes(provider.oauth_status!));
}

export function parseGroupExpansion(raw: string | null): Record<string, boolean> {
  try {
    const value: unknown = JSON.parse(raw ?? "null");
    if (!value || typeof value !== "object" || Array.isArray(value)) return {};
    return Object.fromEntries(Object.entries(value).filter(([, expanded]) => typeof expanded === "boolean"));
  } catch {
    return {};
  }
}

export function isGroupExpanded(group: ProviderGroup, stored: Record<string, boolean>): boolean {
  return Object.prototype.hasOwnProperty.call(stored, group.key) ? stored[group.key] : group.providers.length === 1;
}
