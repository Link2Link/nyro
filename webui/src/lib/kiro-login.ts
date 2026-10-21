import { backend } from "./backend";
import { canReauthorizeLegacyProvider, type AccountPoolView } from "./devin-account-pool";
import type { KiroLoginOptions, OAuthSessionInitData } from "./types";
import type { OAuthSessionContext, OAuthSessionIntent } from "./use-oauth-session";

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

/** Account identity/version are bound at init, never selected after login. */
export async function initKiroAccountSession(intent: OAuthSessionIntent, options: KiroLoginOptions): Promise<OAuthSessionInitData> {
  const kiro = kiroLoginOptions(options);
  if (!kiroLoginConfigured(kiro)) throw new Error("Identity Center start URL and region are required");
  if (intent.kind === "reauthorize_legacy_provider") {
    const pool = await backend<AccountPoolView>("get_provider_account_pool", { id: intent.providerId });
    if (!canReauthorizeLegacyProvider(pool, intent.providerId)) throw new Error("ACCOUNT_VERSION_CONFLICT: provider changed; reopen account management");
    return backend("init_oauth_session", { vendor: "kiro", useProxy: intent.useProxy, kiro });
  }
  return backend("init_provider_account_session", {
    id: intent.providerId, kiro,
    ...(intent.kind === "reauthorize_account" ? { accountId: intent.accountId, expectedVersion: intent.expectedVersion } : {}),
  });
}

/** Revalidate legacy ambiguity and guard every asynchronous bind against cancellation. */
export async function bindKiroAccountSession(context: OAuthSessionContext, label?: string): Promise<boolean> {
  if (!context.isCurrent() || !context.intent) return false;
  const intent = context.intent;
  if (intent.kind === "reauthorize_legacy_provider") {
    const pool = await backend<AccountPoolView>("get_provider_account_pool", { id: intent.providerId }, { signal: context.signal });
    if (!context.isCurrent()) return false;
    if (!canReauthorizeLegacyProvider(pool, intent.providerId)) throw new Error("ACCOUNT_VERSION_CONFLICT: provider changed; reopen account management");
    await backend("bind_provider_oauth", { id: intent.providerId, sessionId: context.sessionId }, { signal: context.signal });
  } else {
    await backend("bind_provider_account", { id: intent.providerId, sessionId: context.sessionId, label }, { signal: context.signal });
  }
  return context.isCurrent();
}
