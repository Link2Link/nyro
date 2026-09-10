// Shared provider-icon loading: used by the HTML badge (components/ui/provider-icon.tsx)
// and by SVG chart markers. Brand resolution itself lives in provider-icon-resolve.ts.
import { useEffect, useState } from "react";
import {
  normalizeProviderSvg,
  resolveProviderIconKey,
  type ProviderIconSource,
} from "./provider-icon-resolve";

const iconModules = import.meta.glob("../assets/icons/*.svg", {
  query: "?raw",
  import: "default",
}) as Record<string, () => Promise<string>>;

const iconLoaderMap: Record<string, () => Promise<string>> = {};
for (const [path, loader] of Object.entries(iconModules)) {
  const matched = path.match(/\/([^/]+)\.svg$/);
  if (matched?.[1]) {
    iconLoaderMap[matched[1].toLowerCase()] = loader;
  }
}
const iconMarkupCache = new Map<string, string>();

/** Own keys only: a provider named "constructor" must not resolve an inherited member. */
const iconKeys = new Set(Object.keys(iconLoaderMap));
const hasIcon = (iconKey: string) => iconKeys.has(iconKey);

/**
 * Resolve a provider's icon key and lazily load its markup. Shared by the HTML
 * badge and by SVG markers, so both render the identical cached icon.
 */
export function useProviderIconMarkup(
  { iconKey, name, baseUrl }: ProviderIconSource,
  monochrome = false,
) {
  const resolvedIconKey = resolveProviderIconKey({ iconKey, name, baseUrl }, hasIcon);
  const cacheKey = resolvedIconKey ? `${resolvedIconKey}:${monochrome ? "mono" : "color"}` : null;
  const [loaded, setLoaded] = useState<{ key: string; markup: string } | null>(null);
  // Cached markup renders immediately; only a genuine miss starts an async load.
  const iconMarkup = cacheKey
    ? (iconMarkupCache.get(cacheKey) ?? (loaded?.key === cacheKey ? loaded.markup : ""))
    : "";

  useEffect(() => {
    if (!cacheKey || iconMarkupCache.has(cacheKey)) return;
    const loader = iconLoaderMap[resolvedIconKey as string];
    if (!loader) return;
    let cancelled = false;
    loader()
      .then((rawSvg) => {
        if (cancelled) return;
        const markup = normalizeProviderSvg(rawSvg, monochrome, resolvedIconKey);
        iconMarkupCache.set(cacheKey, markup);
        setLoaded({ key: cacheKey, markup });
      })
      .catch(() => {
        if (!cancelled) setLoaded(null);
      });

    return () => {
      cancelled = true;
    };
  }, [cacheKey, resolvedIconKey, monochrome]);

  return { iconKey: resolvedIconKey, iconMarkup };
}
