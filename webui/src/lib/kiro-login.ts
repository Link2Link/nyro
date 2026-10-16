import type { KiroLoginOptions } from "./types";

export function kiroLoginConfigured(options: KiroLoginOptions): boolean {
  return options.method !== "idc" || Boolean(options.start_url?.trim() && options.idc_region?.trim());
}

/** Omit empty values so the core owns defaults; do not carry fields between methods. */
export function kiroLoginOptions(options: KiroLoginOptions): KiroLoginOptions {
  if (options.method === "import") return { method: "import" };
  const region = options.region?.trim();
  const idcRegion = options.idc_region?.trim();
  const startUrl = options.start_url?.trim();
  const oidc = options.method === "idc" || options.method === "builder-id";
  return {
    method: options.method,
    ...(region ? { region } : {}),
    ...(oidc && idcRegion ? { idc_region: idcRegion } : {}),
    ...(options.method === "idc" && startUrl ? { start_url: startUrl } : {}),
  };
}
