// Relay-root addressing (the Nyro chain preset, Sub2API): one user-supplied origin
// serves every protocol endpoint of a channel. The backend declares the rules per
// channel (`relay` in the preset metadata, mirroring `RelayAddressing` in
// crates/nyro-core/src/provider/metadata.rs); these helpers let the shared-key form
// collect one address, fan the normalized root out to every protocol endpoint and
// preview each protocol's final route. `relayRoot` mirrors `relay_root` in
// crates/nyro-core/src/provider/sub2api/url.rs, which re-normalizes on save.
//
// Kept free of React so the rules are unit tested directly.

import type { ProviderRelayAddressing } from "./types";

/**
 * Trailing slashes and one known suffix (first match, case-insensitive) are dropped
 * from a pasted address. A reverse-proxy path prefix is kept, and a host named like a
 * suffix (`http://v1`) is never mistaken for a path segment.
 */
export function relayRoot(address: string, suffixes: readonly string[]): string {
  const base = address.trim().replace(/\/+$/, "");
  const lower = base.toLowerCase();
  for (const suffix of suffixes) {
    const needle = suffix.toLowerCase();
    if (lower.length <= needle.length || !lower.endsWith(needle)) continue;
    const rest = base.slice(0, base.length - needle.length);
    // `rest` ending in "/" means the suffix was the host (`http://v1` leaves `http:/`).
    if (!rest || rest.endsWith("/")) continue;
    return rest.replace(/\/+$/, "");
  }
  return base;
}

/** The relay's model listing for an address, or "" while no address is set. */
export function relayModelsSource(address: string, relay: ProviderRelayAddressing): string {
  const root = relayRoot(address, relay.rootSuffixes);
  return root ? `${root}${relay.modelsPath}` : "";
}

/** Extra route prefix the relay expects for a protocol (e.g. Sub2API Antigravity). */
export function relayRoutePrefix(relay: ProviderRelayAddressing, protocol: string): string {
  return relay.routePrefixes?.[protocol] ?? "";
}

const STANDARD_PATHS: Record<string, string> = {
  "openai-compatible": "/v1/chat/completions",
  "openai-responses": "/v1/responses",
  "anthropic-messages": "/v1/messages",
  "google-gemini": "/v1beta/models/{model}",
};

/** Display-only final route of a protocol: root + relay prefix + standard path. */
export function relayRoute(address: string, relay: ProviderRelayAddressing, protocol: string): string {
  const root = relayRoot(address, relay.rootSuffixes);
  if (!root) return "";
  return `${root}${relayRoutePrefix(relay, protocol)}${STANDARD_PATHS[protocol] ?? ""}`;
}

interface RelayForm {
  base_url?: string;
  models_source?: string;
  protocol_endpoints?: Array<{ base_url: string }>;
}

/**
 * Apply a newly typed address: the form keeps the raw text (so typing is never
 * rewritten mid-word), every protocol endpoint gets the normalized root, and the model
 * discovery source follows the address unless the user customized it.
 */
export function applyRelayAddress<T extends RelayForm>(
  form: T,
  address: string,
  relay: ProviderRelayAddressing,
): T {
  const root = relayRoot(address, relay.rootSuffixes);
  const current = (form.models_source ?? "").trim();
  const followsAddress = !current || current === relayModelsSource(form.base_url ?? "", relay);
  return {
    ...form,
    base_url: address,
    protocol_endpoints: form.protocol_endpoints?.map((endpoint) => ({ ...endpoint, base_url: root })),
    models_source: followsAddress ? relayModelsSource(address, relay) : form.models_source,
  } as T;
}

/** Point shared-key picker entries at the current address, so newly checked protocols inherit it. */
export function withRelayAddress<E extends { baseUrl: string }>(
  entries: E[],
  address: string,
  relay: ProviderRelayAddressing,
): E[] {
  const root = relayRoot(address, relay.rootSuffixes);
  return entries.map((entry) => ({ ...entry, baseUrl: root }) as E);
}
