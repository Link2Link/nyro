// Nyro preset (vendor id `nyro-gateway`): chain this gateway to another Nyro instance.
//
// The backend preset (crates/nyro-core/src/provider/nyro_gateway) is a shared-key
// channel that declares every protocol with an empty base URL: a remote Nyro proxy
// serves all of them from one origin, and only the user knows that origin. These
// helpers let the shared-key form collect one address and fan it out to every
// protocol endpoint. The id is not `nyro`: that is the retired Custom preset id the
// storage migrations still rewrite to `custom`, and this page's offline fallback.
//
// Kept free of React so the rules are unit tested directly.

export const NYRO_GATEWAY_PRESET_ID = "nyro-gateway";

export function isNyroGatewayPreset(presetId?: string | null): boolean {
  return presetId?.trim().toLowerCase() === NYRO_GATEWAY_PRESET_ID;
}

/**
 * The remote gateway root: trailing slashes and one trailing `/v1` or `/v1beta`
 * segment are dropped (every remote route already starts with one). A reverse-proxy
 * path prefix is kept. Mirrors `gateway_root` in the backend vendor module.
 */
export function nyroGatewayRoot(address: string): string {
  const base = address.trim().replace(/\/+$/, "");
  const slash = base.lastIndexOf("/");
  if (slash < 0) return base;
  const rest = base.slice(0, slash);
  const last = base.slice(slash + 1).toLowerCase();
  // `rest` ending in "/" means `last` is the host (`http://v1`), not a path segment.
  if ((last === "v1" || last === "v1beta") && !rest.endsWith("/")) {
    return rest.replace(/\/+$/, "");
  }
  return base;
}

/** The remote `/v1/models` listing for an address, or "" while no address is set. */
export function nyroGatewayModelsSource(address: string): string {
  const root = nyroGatewayRoot(address);
  return root ? `${root}/v1/models` : "";
}

interface NyroGatewayForm {
  base_url?: string;
  models_source?: string;
  protocol_endpoints?: Array<{ base_url: string }>;
}

/**
 * Apply a newly typed address: the form keeps the raw text (so typing is never
 * rewritten mid-word), every protocol endpoint gets the normalized root, and the
 * model discovery source follows the address unless the user customized it.
 */
export function applyNyroGatewayAddress<T extends NyroGatewayForm>(form: T, address: string): T {
  const root = nyroGatewayRoot(address);
  const current = (form.models_source ?? "").trim();
  const followsAddress = !current || current === nyroGatewayModelsSource(form.base_url ?? "");
  return {
    ...form,
    base_url: address,
    protocol_endpoints: form.protocol_endpoints?.map((endpoint) => ({ ...endpoint, base_url: root })),
    models_source: followsAddress ? nyroGatewayModelsSource(address) : form.models_source,
  } as T;
}

/** Point shared-key picker entries at the current address, so newly checked protocols inherit it. */
export function withNyroGatewayAddress<E extends { baseUrl: string }>(entries: E[], address: string): E[] {
  const root = nyroGatewayRoot(address);
  return entries.map((entry) => ({ ...entry, baseUrl: root }) as E);
}
