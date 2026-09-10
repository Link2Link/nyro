// Pure provider-icon identity resolution and icon-markup normalization.
//
// Brand identity comes from the provider's canonical key first — the preset/vendor
// key every admin surface already reports as `provider_icon` (preset_key ?? vendor) —
// then from the display name and the API host. The wire protocol never participates:
// values such as "openai-compatible" or "openai-responses" describe a request format,
// so matching them branded every relay as OpenAI regardless of who actually serves it.
//
// Deliberately free of React and bundler globals so the rules are directly unit tested.

/** Preset/vendor keys and common spellings whose icon differs from the key itself.
 *  The `icon:` field of every vendor module under `crates/nyro-core/src/provider/` is
 *  the backend source of truth; every value here must be a shipped icon file name.
 *  The `custom` preset has no usable vector mark: `assets/icons/nyro.svg` renders blank
 *  (its artwork sits outside its own viewBox), so a custom provider keeps whatever its
 *  name and host identify, and otherwise falls back to the provider initial. */
export const ICON_ALIASES: Record<string, string> = {
  // Preset keys declared by vendor metadata.
  "ark-coding": "doubao",
  vertexai: "googlecloud",
  "kimi-code": "kimi",
  moonshotai: "kimi",
  "opencode-go": "opencode-logo-light",
  zhipuai: "zhipu",
  // Model families and third-party spellings that already name a shipped icon.
  claude: "anthropic",
  chatgpt: "openai",
  gpt: "openai",
  googleai: "google",
  googleapis: "google",
  generativelanguage: "gemini",
  tongyi: "qwen",
  dashscope: "qwen",
  modelscope: "modelscope-color",
  aihubmix: "aihubmix-color",
  longcat: "longcat-color",
  moonshot: "kimi",
  hunyuan: "tencent",
  glm: "zhipu",
  chatglm: "zhipu",
  opencode: "opencode-logo-light",
};

export interface ProviderIconSource {
  /** Canonical provider identity: preset key or vendor id. */
  iconKey?: string;
  name?: string;
  baseUrl?: string;
}

/** Whether the build actually ships a loader for this icon key. */
export type IconAvailability = (iconKey: string) => boolean;

/** Lookup form: own keys only, so no provider name can reach an inherited member. */
const OWN_ALIASES = new Map(Object.entries(ICON_ALIASES));

function tokenize(value?: string): string[] {
  if (!value) return [];
  return value
    .toLowerCase()
    .split(/[^a-z0-9]+/g)
    .map((token) => token.trim())
    .filter(Boolean);
}

function hostTokens(baseUrl?: string): string[] {
  if (!baseUrl) return [];
  try {
    return tokenize(new URL(baseUrl).hostname);
  } catch {
    return tokenize(baseUrl);
  }
}

/** An alias is only useful when its target is shipped; otherwise keep the raw key. */
function preferAlias(key: string, available: IconAvailability): string | null {
  if (available(key)) return key;
  const alias = OWN_ALIASES.get(key);
  return alias && available(alias) ? alias : null;
}

/**
 * Resolve the brand icon for one provider, or null when nothing identifies it.
 * An explicit identity outranks name and host, and both outrank the initial fallback
 * the caller renders. Protocol strings are intentionally not an input.
 */
export function resolveProviderIconKey(
  { iconKey, name, baseUrl }: ProviderIconSource,
  available: IconAvailability,
): string | null {
  const explicit = iconKey?.trim().toLowerCase();
  if (explicit) {
    const resolved = preferAlias(explicit, available);
    if (resolved) return resolved;
  }
  for (const token of [...tokenize(name), ...hostTokens(baseUrl)]) {
    const resolved = preferAlias(token, available);
    if (resolved) return resolved;
  }
  return null;
}

const ROOT_SVG = /<svg\b([^>]*)>/i;
/** Sizing and styling attributes the caller owns, never the icon file. */
const CALLER_OWNED = /\s(?:width|height|style|class)\s*=\s*"[^"]*"/gi;

/**
 * Normalize one icon file for injection into any host: the caller decides the box,
 * so the root element is forced to fill it (files declare their own 1em, 179×203 or
 * 240×300 sizes, and inside SVG a nested element's width/height attributes outrank CSS).
 * The viewBox is preserved, so aspect ratio is kept and never stretched.
 */
export function normalizeProviderSvg(svg: string, monochrome: boolean, iconKey?: string | null): string {
  let next = svg
    // Single-quoted attributes would escape every rewrite below.
    .replace(/='([^']*)'/g, '="$1"')
    .replace(/<title>.*?<\/title>/gis, "")
    .replace(
      ROOT_SVG,
      (_match, attributes: string) =>
        `<svg${attributes.replace(CALLER_OWNED, "")} width="100%" height="100%"` +
        ' class="provider-icon-svg" aria-hidden="true" focusable="false">',
    );

  if (!monochrome) return next;

  if (iconKey === "zai") {
    return next
      .replace(/<defs>[\s\S]*?<\/defs>/gi, "")
      .replace(/\sstyle="[^"]*"/gi, "")
      .replace(
        /(<path[^>]*id="zai-bg"[^>]*?)\sfill="[^"]*"/i,
        '$1 fill="none"',
      )
      .replace(
        /(<path[^>]*id="zai-bg"[^>]*?)\sstroke="[^"]*"/i,
        '$1 stroke="currentColor"',
      )
      .replace(
        /(<path[^>]*id="zai-bg"[^>]*?)\sstroke-width="[^"]*"/i,
        '$1 stroke-width="2.1"',
      )
      .replace(
        /(<g[^>]*id="zai-glyph"[^>]*?)\sfill="[^"]*"/i,
        '$1 fill="currentColor"',
      )
      .replace(
        /<svg\b([^>]*)>/i,
        '<svg$1 fill="currentColor" stroke="currentColor" color="currentColor">',
      );
  }

  next = next
    .replace(/<defs>[\s\S]*?<\/defs>/gi, "")
    .replace(/\sfill="(?!none)[^"]*"/gi, ' fill="currentColor"')
    .replace(/\sstroke="(?!none)[^"]*"/gi, ' stroke="currentColor"')
    .replace(/\sstop-color="[^"]*"/gi, ' stop-color="currentColor"')
    .replace(/\sstyle="[^"]*"/gi, "")
    .replace(
      /<svg\b([^>]*)>/i,
      '<svg$1 fill="currentColor" stroke="currentColor" color="currentColor">',
    );

  return next;
}
