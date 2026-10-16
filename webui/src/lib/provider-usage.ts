import type { Provider, ProviderUsage } from "./types";
import { nativeProviderProtocol } from "./native-provider";

/** Providers whose vendor has a coding-plan usage-query backend. Mirrors
 * nyro-core `admin::usage::UsageBackend::detect`. */
export function providerUsageSupported(
  provider: Pick<Provider, "base_url" | "vendor" | "preset_key" | "channel" | "auth_mode">,
): boolean {
  const url = provider.base_url.toLowerCase();
  const vendor = (provider.vendor ?? provider.preset_key ?? "").toLowerCase();
  const isCodexOAuth =
    provider.auth_mode === "oauth" &&
    provider.channel?.toLowerCase() === "codex" &&
    (vendor === "openai" || vendor === "codex");
  const isImportedCodexOAuth =
    provider.auth_mode === "oauth" && url.includes("chatgpt.com/backend-api/codex");
  const isGrokOAuth =
    provider.auth_mode === "oauth" &&
    provider.channel?.toLowerCase() === "grok" &&
    (vendor === "xai" || vendor === "grok");
  const isImportedGrokOAuth =
    provider.auth_mode === "oauth" && url.includes("cli-chat-proxy.grok.com");
  const isGoogleOAuth =
    provider.auth_mode === "oauth" &&
    vendor === "google" &&
    (provider.channel?.toLowerCase() === "antigravity" ||
      provider.channel?.toLowerCase() === "gemini-cli");
  const isImportedGoogleOAuth =
    provider.auth_mode === "oauth" && url.includes("cloudcode-pa.googleapis.com");
  // Native subscription providers are recognised by identity, not a shared
  // Codeium/AWS host (mirrors `UsageBackend::detect`).
  const isNativeOAuth = provider.auth_mode === "oauth" && nativeProviderProtocol(provider) !== null;
  return (
    isCodexOAuth ||
    isImportedCodexOAuth ||
    isGrokOAuth ||
    isImportedGrokOAuth ||
    isGoogleOAuth ||
    isImportedGoogleOAuth ||
    isNativeOAuth ||
    url.includes("bigmodel.cn") ||
    url.includes("z.ai") ||
    url.includes("minimaxi.com") ||
    url.includes("minimax.io") ||
    url.includes("api.kimi.com") ||
    url.includes("api.deepseek.com") ||
    url.includes("opencode.ai/zen") ||
    url.includes("volces.com") ||
    // MiMo cluster hosts (`token-plan-<cluster>.xiaomimimo.com`) end in
    // `-cn.`/`-sgp.` rather than Bailian's `token-plan.`, so the vendor
    // domain must be matched explicitly (mirrors `UsageBackend::detect`,
    // which checks xiaomimimo before the `token-plan.` fallback).
    url.includes("xiaomimimo.com") ||
    url.includes("token-plan.")
  );
}


/** The backend keeps an exhausted Kiro total pool eligible only when upstream
 * explicitly enabled overage. Feature/detail rows never decide availability. */
export function isKiroOverage(usage?: Pick<ProviderUsage, "kind" | "scheduling" | "is_available" | "tiers"> | null): boolean {
  return usage?.kind === "kiro_plan"
    && usage.scheduling?.status === "eligible"
    && usage.is_available !== false
    && usage.tiers.some((tier) => !tier.name.startsWith("feature:") && tier.used_percent >= 100);
}

export function providerUsageTitle(kind: string | undefined, isZh: boolean): string {
  if (kind?.endsWith("_balance")) return isZh ? "账户余额" : "Balance";
  if (kind === "openai_codex") return isZh ? "ChatGPT Codex 用量" : "ChatGPT Codex Usage";
  if (kind === "devin_plan") return isZh ? "Devin 用量" : "Devin Usage";
  if (kind === "kiro_plan") return isZh ? "Kiro 用量" : "Kiro Usage";
  if (kind === "google_subscription") return isZh ? "Google AI Pro 配额" : "Google AI Pro Quota";
  return isZh ? "套餐用量" : "Plan Usage";
}

const TIER_LABELS_ZH: Record<string, string> = {
  five_hour: "5小时",
  daily: "每日",
  weekly_limit: "每周",
  monthly: "每月",
  primary_window: "主要窗口",
  secondary_window: "次要窗口",
  gemini: "Gemini 共享池",
};
const TIER_LABELS_EN: Record<string, string> = {
  five_hour: "5h",
  daily: "Daily",
  weekly_limit: "Weekly",
  monthly: "Monthly",
  primary_window: "Primary Window",
  secondary_window: "Secondary Window",
  gemini: "Gemini Shared Pool",
};

function readableFeatureName(value: string): string {
  return value
    .replace(/[_-]+/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .replace(/\b\w/g, (character) => character.toUpperCase());
}

export function tierLabel(name: string, isZh: boolean): string {
  const table = isZh ? TIER_LABELS_ZH : TIER_LABELS_EN;
  if (table[name]) return table[name];

  const kiro = /^feature:kiro:(base|bonus|free_trial):(.+):\d+$/.exec(name);
  if (kiro) {
    const kind = kiro[1] === "base" ? (isZh ? "基础额度" : "Base allowance")
      : kiro[1] === "bonus" ? (isZh ? "赠送额度" : "Bonus") : (isZh ? "免费试用" : "Free trial");
    return `${kind} · ${readableFeatureName(kiro[2])}`;
  }
  if (name.startsWith("kiro:")) return `${readableFeatureName(name.slice(5))} · ${isZh ? "总额度" : "Total pool"}`;
  const feature = /^feature:(.+):(five_hour|daily|weekly_limit|monthly|primary_window|secondary_window)$/.exec(name);
  if (feature) {
    const featureName = readableFeatureName(feature[1]);
    const windowLabel = table[feature[2]] ?? feature[2];
    return `${featureName} · ${windowLabel}`;
  }
  return readableFeatureName(name);
}
