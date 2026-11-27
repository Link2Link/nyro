// Nyro preset (vendor id `nyro-gateway`): chain this gateway to another Nyro instance.
//
// The backend preset (crates/nyro-core/src/provider/nyro_gateway) is a shared-key
// channel that declares every protocol with an empty base URL plus relay-root
// addressing: a remote Nyro proxy serves all of them from one origin, and only the
// user knows that origin. The generic relay helpers (./relay-address) collect one
// address and fan it out to every protocol endpoint; these wrappers pin the Nyro
// rules. The id is not `nyro`: that is the retired Custom preset id the storage
// migrations still rewrite to `custom`, and this page's offline fallback.
//
// Kept free of React so the rules are unit tested directly.

import { applyRelayAddress, relayModelsSource, relayRoot, withRelayAddress } from "./relay-address";
import type { ProviderRelayAddressing } from "./types";

export const NYRO_GATEWAY_PRESET_ID = "nyro-gateway";

/** Mirrors the `relay` metadata of the backend Nyro preset. */
export const NYRO_GATEWAY_RELAY: ProviderRelayAddressing = {
  modelsPath: "/v1/models",
  rootSuffixes: ["/v1beta", "/v1"],
};

export function isNyroGatewayPreset(presetId?: string | null): boolean {
  return presetId?.trim().toLowerCase() === NYRO_GATEWAY_PRESET_ID;
}

/**
 * The remote gateway root: trailing slashes and one trailing `/v1` or `/v1beta`
 * segment are dropped (every remote route already starts with one). A reverse-proxy
 * path prefix is kept. Mirrors `gateway_root` in the backend vendor module.
 */
export function nyroGatewayRoot(address: string): string {
  return relayRoot(address, NYRO_GATEWAY_RELAY.rootSuffixes);
}

/** The remote `/v1/models` listing for an address, or "" while no address is set. */
export function nyroGatewayModelsSource(address: string): string {
  return relayModelsSource(address, NYRO_GATEWAY_RELAY);
}

interface NyroGatewayForm {
  base_url?: string;
  models_source?: string;
  protocol_endpoints?: Array<{ base_url: string }>;
}

/** See {@link applyRelayAddress}. */
export function applyNyroGatewayAddress<T extends NyroGatewayForm>(form: T, address: string): T {
  return applyRelayAddress(form, address, NYRO_GATEWAY_RELAY);
}

/** Point shared-key picker entries at the current address, so newly checked protocols inherit it. */
export function withNyroGatewayAddress<E extends { baseUrl: string }>(entries: E[], address: string): E[] {
  return withRelayAddress(entries, address, NYRO_GATEWAY_RELAY);
}
