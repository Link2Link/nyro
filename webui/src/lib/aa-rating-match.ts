import type { AaModelAlias, AaModelRatingEntry, EffectiveRatingSource, ModelRatingEntry } from "./types";
import { canonicalModelPrefix, longestRatingMatch } from "./model-ratings";

const STRIPPABLE_COLON_TAGS = new Set([
  "free", "exacto", "thinking", "extended", "nitro", "floor", "online", "beta", "latest",
]);

const GLUED_BRAND_PREFIXES = [
  "qwen", "llama", "gemma", "glm", "lfm", "olmo", "step", "solar", "minicpm", "mistral",
  "ministral", "devstral", "mixtral", "granite", "jamba", "exaone", "ling", "ring",
];

const OPTIONAL_VENDOR_PREFIXES = new Set([
  "nvidia", "tencent", "alibaba", "meta", "google", "openai", "anthropic", "xai", "deepseek",
  "mistral", "moonshotai", "zhipuai", "minimax", "xiaomi", "amazon", "cohere",
]);

const PROTECTED_PRODUCT_TOKENS = new Set([
  "pro", "flash", "lite", "mini", "nano", "micro", "tiny", "small", "large", "ultra", "turbo",
  "fast", "codex", "spark", "omni", "vl", "vision", "code", "coder", "audio", "tts", "stt",
  "image", "embedding", "distill", "air", "plus", "speciale", "terminus", "multi-agent",
  "deep-think", "deep-research", "sol", "terra", "luna", "astra", "argon", "carbon", "hydrogen",
  "neon", "opus", "sonnet", "haiku", "fable", "scout", "maverick", "instant", "next", "open",
  "full", "super", "bolt", "lightning",
]);

const MONTHS = new Set([
  "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
]);

const CLAUDE_TIERS = new Set(["opus", "sonnet", "haiku", "fable"]);

const SINGLE_MODIFIERS = new Set([
  "max", "xhigh", "high", "medium", "med", "low", "minimal", "xlow", "none", "off",
  "tiered", "reasoning", "thinking", "think", "adaptive", "extended", "default",
  "latest", "preview", "beta", "exp", "experimental", "highspeed",
  "1m", "2m", "128k", "256k", "instruct", "chat", "customtools",
]);

const PAIR_MODIFIERS = new Set([
  "build-fast", "extra-high", "extra-low", "non-reasoning", "no-reasoning",
  "non-thinking", "no-thinking", "chat-latest", "default-fallback", "fast-latest",
]);

function isDigits(s: string): boolean {
  return /^[0-9]+$/.test(s);
}

function stripOuterNoise(raw: string): string {
  let s = raw.trim();
  const slash = s.lastIndexOf("/");
  if (slash >= 0 && s.slice(slash + 1).trim()) {
    s = s.slice(slash + 1).trim();
  }
  const colon = s.lastIndexOf(":");
  if (colon > 0 && STRIPPABLE_COLON_TAGS.has(s.slice(colon + 1).trim().toLowerCase()) && s.slice(0, colon).trim()) {
    s = s.slice(0, colon).trim();
  }
  let out = "";
  let depth = 0;
  for (const ch of s) {
    if (ch === "(") depth += 1;
    else if (ch === ")") { if (depth > 0) depth -= 1; }
    else if (depth === 0) out += ch;
  }
  return out.trim();
}

function tokenizeModelName(raw: string): string[] {
  const cleaned = stripOuterNoise(raw).toLowerCase();
  const tokens: string[] = [];
  let current = "";
  for (let i = 0; i < cleaned.length; i += 1) {
    const ch = cleaned[i];
    const prev = i > 0 ? cleaned.charCodeAt(i - 1) : 0;
    const next = i + 1 < cleaned.length ? cleaned.charCodeAt(i + 1) : 0;
    const isNumericDot = ch === "." && prev >= 48 && prev <= 57 && next >= 48 && next <= 57;
    if (/\s/.test(ch) || ch === "-" || ch === "_" || isNumericDot) {
      if (current) { tokens.push(current); current = ""; }
    } else {
      current += ch;
    }
  }
  if (current) tokens.push(current);
  return tokens;
}

function canonicalizeTokens(input: string[]): string[] {
  const tokens = [...input];
  if (tokens.length === 0) return tokens;
  const first = tokens[0];
  for (const brand of GLUED_BRAND_PREFIXES) {
    if (first.startsWith(brand)) {
      const rest = first.slice(brand.length);
      if (isDigits(rest)) {
        tokens[0] = brand;
        tokens.splice(1, 0, rest);
        break;
      }
    }
  }
  if (tokens[0] === "claude") {
    if (tokens.length >= 2 && tokens[1].length === 2 && isDigits(tokens[1])) {
      const d1 = tokens[1][0];
      const d2 = tokens[1][1];
      if (d1 >= "2" && d1 <= "6") {
        tokens[1] = d1;
        tokens.splice(2, 0, d2);
      }
    }
    if (tokens.length >= 3 && isDigits(tokens[1]) && CLAUDE_TIERS.has(tokens[2])) {
      [tokens[1], tokens[2]] = [tokens[2], tokens[1]];
    } else if (tokens.length >= 4 && isDigits(tokens[1]) && isDigits(tokens[2]) && CLAUDE_TIERS.has(tokens[3])) {
      const [tier] = tokens.splice(3, 1);
      tokens.splice(1, 0, tier);
    }
  }
  return tokens;
}

export function normalizeModelKey(raw: string): string {
  return canonicalizeTokens(tokenizeModelName(raw)).join("-");
}

function dateLikeSuffixLen(tokens: string[]): number {
  const n = tokens.length;
  if (n < 2) return 0;
  const last = tokens[n - 1];
  if (n >= 4 && tokens[n - 3].length === 4 && isDigits(tokens[n - 3]) && tokens[n - 3].startsWith("20")
    && tokens[n - 2].length === 2 && isDigits(tokens[n - 2]) && last.length === 2 && isDigits(last)) {
    return 3;
  }
  if (n >= 3) {
    const prev = tokens[n - 2];
    const mmYyyy = prev.length === 2 && isDigits(prev) && last.length === 4 && isDigits(last) && last.startsWith("20");
    const mmDd = prev.length === 2 && isDigits(prev) && last.length === 2 && isDigits(last)
      && Number(prev) >= 1 && Number(prev) <= 12 && Number(last) >= 1 && Number(last) <= 31;
    const monYear = MONTHS.has(prev) && (last.length === 2 || (last.length === 4 && last.startsWith("20"))) && isDigits(last);
    if (mmYyyy || mmDd || monYear) return 2;
  }
  if (last.length === 8 && isDigits(last) && last.startsWith("20")) return 1;
  if (last.length === 4 && isDigits(last)) {
    const a = Number(last.slice(0, 2));
    const b = Number(last.slice(2, 4));
    if ((a >= 1 && a <= 12 && b >= 1 && b <= 31) || (a >= 23 && a <= 35 && b >= 1 && b <= 12)) return 1;
  }
  if (last.length === 3 && isDigits(last) && last.startsWith("0")) return 1;
  if (last.length >= 4 && last.length <= 5 && MONTHS.has(last.slice(0, 3)) && isDigits(last.slice(3))) return 1;
  return 0;
}

function stripOneModifierSuffix(tokens: string[]): string[] | null {
  const n = tokens.length;
  if (n < 2) return null;
  if (n >= 3 && PAIR_MODIFIERS.has(`${tokens[n - 2]}-${tokens[n - 1]}`)) {
    return tokens.slice(0, n - 2);
  }
  const dlen = dateLikeSuffixLen(tokens);
  if (dlen > 0 && n > dlen) return tokens.slice(0, n - dlen);
  const last = tokens[n - 1];
  if (PROTECTED_PRODUCT_TOKENS.has(last)) return null;
  if (SINGLE_MODIFIERS.has(last)) {
    if (n === 2 && (last === "chat" || last === "instruct" || last === "thinking" || last === "reasoning")) return null;
    return tokens.slice(0, n - 1);
  }
  return null;
}

export interface AaLookupIndex {
  primary: Map<string, AaModelRatingEntry>;
  secondary: Map<string, AaModelRatingEntry>;
  entries: AaModelRatingEntry[];
}

export function buildAaLookupIndex(entries: AaModelRatingEntry[]): AaLookupIndex {
  const primary = new Map<string, AaModelRatingEntry>();
  for (const entry of entries) primary.set(entry.canonical_key, entry);
  const secondary = new Map<string, AaModelRatingEntry>();
  const ambiguous = new Set<string>();
  function register(key: string, entry: AaModelRatingEntry) {
    if (!key || primary.has(key) || ambiguous.has(key)) return;
    const existing = secondary.get(key);
    if (existing && existing.canonical_key !== entry.canonical_key) {
      secondary.delete(key);
      ambiguous.add(key);
    } else {
      secondary.set(key, entry);
    }
  }
  for (const entry of entries) {
    const tokens = entry.canonical_key.split("-");
    const stripped = [...tokens];
    let changed = false;
    while (stripped.length > 0) {
      const last = stripped[stripped.length - 1];
      if (last === "preview" || last === "beta" || last === "exp" || last === "experimental" || last === "instruct") {
        stripped.pop();
        changed = true;
      } else {
        const dlen = dateLikeSuffixLen(stripped);
        if (dlen > 0 && stripped.length > dlen) {
          stripped.splice(stripped.length - dlen, dlen);
          changed = true;
        } else break;
      }
    }
    if (changed && stripped.length >= 2) register(stripped.join("-"), entry);
    if (tokens.length >= 3 && OPTIONAL_VENDOR_PREFIXES.has(tokens[0])) {
      register(tokens.slice(1).join("-"), entry);
    }
    const nameKey = normalizeModelKey(entry.aa_name);
    if (nameKey !== entry.canonical_key) register(nameKey, entry);
  }
  return { primary, secondary, entries };
}

function matchSequence(index: AaLookupIndex, initial: string[]): AaModelRatingEntry | null {
  let current = [...initial];
  for (let i = 0; i < 8; i += 1) {
    const key = current.join("-");
    const hit = index.primary.get(key) ?? index.secondary.get(key);
    if (hit) return hit;
    const next = stripOneModifierSuffix(current);
    if (!next || next.length === 0) break;
    const canon = canonicalizeTokens(next);
    if (canon.join("-") === key) break;
    current = canon;
  }
  return null;
}

export function matchAaModel(model: string, index: AaLookupIndex): AaModelRatingEntry | null {
  const tokens = canonicalizeTokens(tokenizeModelName(model));
  if (tokens.length === 0) return null;
  const direct = matchSequence(index, tokens);
  if (direct) return direct;
  if (tokens.length >= 3 && OPTIONAL_VENDOR_PREFIXES.has(tokens[0])) {
    return matchSequence(index, canonicalizeTokens(tokens.slice(1)));
  }
  return null;
}

export interface EffectiveModelRating {
  model_prefix: string;
  score: number;
  raw_score: number | null;
  updated_at: string;
  source: EffectiveRatingSource;
  aa_entry?: AaModelRatingEntry;
}

export function resolveEffectiveRating(
  model: string,
  manualEntries: ModelRatingEntry[],
  aaAliases: AaModelAlias[],
  aaIndex: AaLookupIndex,
  aaSyncedAt: string | null,
): EffectiveModelRating | null {
  const directManual = longestRatingMatch(model, manualEntries);
  if (directManual) {
    return {
      model_prefix: canonicalModelPrefix(directManual.model_prefix),
      score: directManual.score,
      raw_score: null,
      updated_at: directManual.updated_at,
      source: "manual",
    };
  }
  const normalized = normalizeModelKey(model);
  if (normalized && normalized !== canonicalModelPrefix(model)) {
    const normalizedManual = longestRatingMatch(normalized, manualEntries);
    if (normalizedManual) {
      return {
        model_prefix: canonicalModelPrefix(normalizedManual.model_prefix),
        score: normalizedManual.score,
        raw_score: null,
        updated_at: normalizedManual.updated_at,
        source: "manual",
      };
    }
  }
  if (aaAliases.length > 0) {
    const aliasEntries: ModelRatingEntry[] = aaAliases.map((a) => ({
      model_prefix: a.model_prefix,
      score: 0,
      updated_at: a.updated_at,
    }));
    const matched = longestRatingMatch(model, aliasEntries)
      ?? (normalized ? longestRatingMatch(normalized, aliasEntries) : null);
    if (matched) {
      const alias = aaAliases.find((a) => a.model_prefix === matched.model_prefix);
      const aaEntry = alias ? aaIndex.primary.get(alias.aa_canonical_key) : undefined;
      if (alias && aaEntry) {
        return {
          model_prefix: canonicalModelPrefix(alias.model_prefix),
          score: aaEntry.score,
          raw_score: aaEntry.raw_score,
          updated_at: aaSyncedAt ?? alias.updated_at,
          source: "aa_alias",
          aa_entry: aaEntry,
        };
      }
    }
  }
  const auto = matchAaModel(model, aaIndex);
  if (!auto) return null;
  return {
    model_prefix: auto.canonical_key,
    score: auto.score,
    raw_score: auto.raw_score,
    updated_at: aaSyncedAt ?? "",
    source: "aa_auto",
    aa_entry: auto,
  };
}
