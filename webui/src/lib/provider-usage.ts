import type { NyroUpstreamProvider, NyroUpstreamUsage, Provider, ProviderUsage } from "./types";
import { nativeProviderProtocol } from "./native-provider";
import { accountStateLabel, type PoolQuotaWindow } from "./devin-account-pool";
import { isNyroGatewayPreset } from "./nyro-gateway";

export interface ProviderUsagePause {
  message: string;
  quotaExhausted: boolean;
  resetAt: string | null;
}

/** `quota_exhausted` is the legacy routing stop flag, not necessarily a quota
 * diagnosis. Prefer explicit account failures over quota-looking cached tiers.
 * A polling deadline is never a quota reset or a promised recovery time. */
export function providerUsagePause(
  usage?: Pick<ProviderUsage, "scheduling" | "account_pool"> | null,
  isZh = false,
): ProviderUsagePause | null {
  const scheduling = usage?.scheduling;
  if (scheduling?.status !== "quota_exhausted") return null;

  const reason = scheduling.reason?.trim();
  const readiness = usage?.account_pool?.readiness?.trim();
  const isQuota = (value?: string) => value === "usage_limit" || value === "quota_exhausted";
  const generic = !reason || reason === "account_unavailable" || reason === "no_available_account";
  // Eligible/read-only pool states must not replace an actual pause diagnosis.
  const poolReason = readiness && !["eligible", "included", "reserve", "reserve_only"].includes(readiness)
    ? readiness : undefined;
  const effectiveReason = (generic || isQuota(reason)) && poolReason ? poolReason : reason;
  const quotaExhausted = isQuota(effectiveReason)
    || (!effectiveReason && scheduling.blocking_tiers.length > 0);

  let message: string;
  if (quotaExhausted) {
    message = isZh ? "额度已满，已暂停调度" : "Quota exhausted, routing paused";
  } else {
    const fallback = isZh ? "账号不可用" : "Account unavailable";
    const label = !effectiveReason || ["account_unavailable", "no_available_account"].includes(effectiveReason)
      ? fallback : accountStateLabel(effectiveReason, isZh);
    message = isZh ? `${label}，已暂停调度` : `${label}, routing paused`;
  }
  return {
    message,
    quotaExhausted,
    resetAt: quotaExhausted ? scheduling.reset_at ?? null : null,
  };
}

type UsageIdentity = Pick<Provider, "base_url" | "vendor" | "preset_key" | "channel" | "auth_mode">;

/** Usage computed from this gateway's retained logs, not an upstream quota observation. */
export type LocalUsageKind = "sub2api" | "anthropic_api";

export function isLocalUsageKind(kind: string | null | undefined): kind is LocalUsageKind {
  return kind === "sub2api" || kind === "anthropic_api";
}

/** Anthropic API-key default channel by identity; a blank legacy channel is `default`. */
function isAnthropicDefaultChannel(provider: Pick<Provider, "vendor" | "preset_key" | "channel">): boolean {
  const identities = [provider.vendor, provider.preset_key]
    .map((value) => value?.trim().toLowerCase())
    .filter((value): value is string => Boolean(value));
  const channel = provider.channel?.trim().toLowerCase();
  return identities.length > 0 && identities.every((value) => value === "anthropic")
    && (!channel || channel === "default");
}

/** Local accounting kind. Mirrors nyro-core `admin::usage::LocalUsageAccounting::detect`. */
export function localUsageKind(provider: UsageIdentity): LocalUsageKind | null {
  if ([provider.vendor, provider.preset_key].some(isNyroGatewayPreset)) return null;
  if (provider.channel?.trim().toLowerCase() === "sub2api") return "sub2api";
  // A vendor quota backend selected by Base URL keeps its upstream view.
  if (isAnthropicDefaultChannel(provider) && !upstreamUsageSupported(provider)) return "anthropic_api";
  return null;
}

/** Providers with an upstream usage-query backend. Mirrors
 * nyro-core `admin::usage::UsageBackend::detect`. */
export function providerUsageSupported(provider: UsageIdentity): boolean {
  return localUsageKind(provider) === null && upstreamUsageSupported(provider);
}

function upstreamUsageSupported(provider: UsageIdentity): boolean {
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
  // Claude subscription: identity only — api.anthropic.com also serves
  // API-key providers (mirrors `UsageBackend::detect`).
  const isClaudeCodeOAuth =
    provider.auth_mode === "oauth" &&
    provider.channel?.toLowerCase() === "claude-code" &&
    vendor === "anthropic";
  // Native subscription providers are recognised by identity, not a shared
  // Codeium/AWS host (mirrors `UsageBackend::detect`).
  const isNativeOAuth = provider.auth_mode === "oauth" && nativeProviderProtocol(provider) !== null;
  // A chained Nyro forwards its upstream usage; its address is any host.
  const isNyroGateway = [provider.vendor, provider.preset_key].some(isNyroGatewayPreset);
  return (
    isNyroGateway ||
    isCodexOAuth ||
    isImportedCodexOAuth ||
    isGrokOAuth ||
    isImportedGrokOAuth ||
    isGoogleOAuth ||
    isImportedGoogleOAuth ||
    isClaudeCodeOAuth ||
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

/** Provider-level API-equivalent totals are currently emitted for these plan backends only. */
export function providerApiPriceSupported(kind: string | undefined): boolean {
  return kind === "glm_coding_plan" || kind === "minimax_coding_plan" || kind === "grok_plan" || kind === "openai_codex"
    || kind === "google_subscription";
}

export function providerUsageTitle(kind: string | undefined, isZh: boolean): string {
  if (kind === "sub2api") return isZh ? "Sub2API 用量" : "Sub2API Usage";
  if (kind === "anthropic_api") return isZh ? "Anthropic API 用量" : "Anthropic API Usage";
  if (kind?.endsWith("_balance")) return isZh ? "账户余额" : "Balance";
  if (kind === "openai_codex") return isZh ? "ChatGPT Codex 用量" : "ChatGPT Codex Usage";
  if (kind === "devin_plan") return isZh ? "Devin 用量" : "Devin Usage";
  if (kind === "kiro_plan") return isZh ? "Kiro 用量" : "Kiro Usage";
  if (kind === "google_subscription") return isZh ? "Google AI Pro 配额" : "Google AI Pro Quota";
  if (kind === "claude_code_plan") return isZh ? "Claude 订阅用量" : "Claude Subscription Usage";
  if (kind === "nyro_gateway") return isZh ? "上游 Nyro 用量" : "Upstream Nyro Usage";
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
  nyro_upstream: "上游供应商全部暂停",
};
const TIER_LABELS_EN: Record<string, string> = {
  five_hour: "5h",
  daily: "Daily",
  weekly_limit: "Weekly",
  monthly: "Monthly",
  primary_window: "Primary Window",
  secondary_window: "Secondary Window",
  gemini: "Gemini Shared Pool",
  nyro_upstream: "All upstream providers paused",
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

/** One group member's usage query state, as seen by the group header. */
export interface GroupUsageEntry {
  name: string;
  usage?: ProviderUsage | null;
  failed?: boolean;
  loading?: boolean;
}

export interface GroupUsageSummary {
  /** Members that have a usage backend and are being queried. */
  tracked: number;
  /** Highest main-window used percent across members (0-100), if any reported. */
  peak: { name: string; tier: string; percent: number } | null;
  /** Per-member worst main-window percent, for the tooltip. */
  perProvider: { name: string; percent: number }[];
  paused: number;
  unavailable: number;
  failed: number;
  loading: number;
  /** Balance totals per currency, only for members without percent tiers. */
  balances: { currency: string; total: number }[];
}

/** Anonymous windows are already aggregated by core; never infer capacity from route membership. */
function nyroUpstreamPoolCandidates(usage?: Pick<ProviderUsage, "pool_quota"> | null): [string, PoolQuotaWindow][] {
  const pool = usage?.pool_quota;
  if (!pool) return [];
  const candidates: [string, PoolQuotaWindow][] = [];
  if (pool.daily) candidates.push(["daily", pool.daily]);
  if (pool.weekly) candidates.push(["weekly_limit", pool.weekly]);
  for (const window of pool.claude_windows ?? []) {
    if ((window.name === "five_hour" || window.name === "weekly_limit") && window.quota) {
      candidates.push([window.name, window.quota]);
    }
  }
  return candidates;
}

/** Presence also includes conflicts, so an old flat tier cannot hide unknown pool usage. */
export const nyroUpstreamPoolWindowNames = (usage?: Pick<ProviderUsage, "pool_quota"> | null) =>
  new Set(nyroUpstreamPoolCandidates(usage).map(([name]) => name));

export function nyroUpstreamPoolWindows(usage?: Pick<ProviderUsage, "pool_quota"> | null): [string, PoolQuotaWindow][] {
  const candidates = nyroUpstreamPoolCandidates(usage);
  return ["five_hour", "daily", "weekly_limit"].flatMap((name): [string, PoolQuotaWindow][] => {
    const matching = candidates.filter(([key]) => key === name).map(([, quota]) => quota);
    const first = matching[0];
    // Conflicting duplicates are unknown, never an optimistic first/max observation.
    if (!first || matching.some((quota) => Object.keys(first).some((field) =>
      quota[field as keyof PoolQuotaWindow] !== first[field as keyof PoolQuotaWindow]))) return [];
    return [[name, first]];
  });
}

/** A partial/stale pool must not look like a fresh 0% or a smaller complete pool. */
export function nyroUpstreamPoolUsedPercent(window: PoolQuotaWindow): number | null {
  const used = window.used_percent;
  return window.capacity_complete && !window.stale && Number.isFinite(window.total_units) && window.total_units > 0
    && window.total_accounts > 0 && window.known_accounts === window.total_accounts
    && used != null && Number.isFinite(used) && used >= 0 && used <= 100 ? used : null;
}

/** Worst main-window observation, for display only. Feature tiers never decide the summary. */
function worstMainTier(usage: ProviderUsage): { tier: string; percent: number } | null {
  let worst: { tier: string; percent: number } | null = null;
  const observations = usage.upstream
    ? usage.upstream.providers.flatMap((provider) => provider.status === "ok" && provider.usage ? [provider.usage] : [])
    : [usage];
  for (const observation of observations) {
    const windows = nyroUpstreamPoolWindows(observation);
    const poolNames = nyroUpstreamPoolWindowNames(observation);
    const tiers = (observation.tiers ?? []).filter((tier) => !poolNames.has(tier.name));
    for (const [name, window] of windows) {
      const used = nyroUpstreamPoolUsedPercent(window);
      if (used != null) tiers.push({ name, used_percent: used });
    }
    for (const tier of tiers) {
      if (tier.name.startsWith("feature:") || !Number.isFinite(tier.used_percent)) continue;
      const percent = Math.min(Math.max(tier.used_percent, 0), 100);
      if (!worst || percent > worst.percent) worst = { tier: tier.name, percent };
    }
  }
  return worst;
}

export function summarizeGroupUsage(entries: GroupUsageEntry[]): GroupUsageSummary {
  const summary: GroupUsageSummary = {
    tracked: entries.length, peak: null, perProvider: [],
    paused: 0, unavailable: 0, failed: 0, loading: 0, balances: [],
  };
  const totals = new Map<string, number>();
  for (const { name, usage, failed, loading } of entries) {
    if (!usage) {
      if (failed) summary.failed += 1;
      else if (loading) summary.loading += 1;
      continue;
    }
    if (usage.scheduling?.status === "quota_exhausted") summary.paused += 1;
    if (usage.is_available === false) summary.unavailable += 1;
    const worst = worstMainTier(usage);
    if (worst) {
      summary.perProvider.push({ name, percent: worst.percent });
      if (!summary.peak || worst.percent > summary.peak.percent) summary.peak = { name, ...worst };
    } else if (!usage.upstream) {
      for (const balance of usage.balances ?? []) {
        if (Number.isFinite(balance.total)) totals.set(balance.currency, (totals.get(balance.currency) ?? 0) + balance.total);
      }
    }
  }
  summary.balances = [...totals].map(([currency, total]) => ({ currency, total }));
  return summary;
}

/** Upstream provider rows of a forwarded Nyro report, ordered by urgency. */
export interface NyroUpstreamView {
  /** Providers with usage data or a query error, most urgent first. */
  shown: NyroUpstreamProvider[];
  /** Names of upstream providers without a usage backend on the remote. */
  unsupported: string[];
  total: number;
  paused: number;
  failed: number;
  /** Money fields the remote withheld from this key. */
  moneyRedacted: boolean;
}

function upstreamRank(provider: NyroUpstreamProvider): [number, number] {
  if (provider.status === "ok" && provider.usage?.scheduling?.status === "quota_exhausted") return [0, 0];
  if (provider.status === "ok") {
    const worst = provider.usage ? worstMainTier(provider.usage) : null;
    return [provider.usage?.is_available === false ? 1 : 2, -(worst?.percent ?? -1)];
  }
  return [3, 0];
}

/** Paused first, then the most used, then query failures; unsupported last as names. */
export function nyroUpstreamView(report?: NyroUpstreamUsage | null): NyroUpstreamView {
  const providers = report?.providers ?? [];
  const shown = providers
    .filter((provider) => provider.status !== "unsupported")
    .map((provider, index) => ({ provider, index, rank: upstreamRank(provider) }))
    .sort((a, b) => a.rank[0] - b.rank[0] || a.rank[1] - b.rank[1] || a.index - b.index)
    .map(({ provider }) => provider);
  return {
    shown,
    unsupported: providers.filter((provider) => provider.status === "unsupported").map((provider) => provider.name),
    total: providers.length,
    paused: providers.filter((provider) => upstreamRank(provider)[0] === 0).length,
    failed: providers.filter((provider) => provider.status === "error").length,
    moneyRedacted: (report?.redacted ?? []).some((field) => field === "balances" || field === "spends"),
  };
}
