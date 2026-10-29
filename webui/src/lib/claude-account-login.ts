import { backend } from "./backend";
import { canReauthorizeLegacyProvider, type AccountPoolView } from "./devin-account-pool";
import type { OAuthSessionInitData } from "./types";
import type { OAuthSessionContext, OAuthSessionIntent } from "./use-oauth-session";

/** The backend records the add/targeted-replace intent before opening the browser. */
export async function initClaudeAccountSession(intent: OAuthSessionIntent): Promise<OAuthSessionInitData> {
  if (intent.kind === "reauthorize_legacy_provider") {
    const pool = await backend<AccountPoolView>("get_provider_account_pool", { id: intent.providerId });
    if (!canReauthorizeLegacyProvider(pool, intent.providerId)) throw new Error("ACCOUNT_VERSION_CONFLICT: provider changed; reopen account management");
    return backend("init_oauth_session", { vendor: "claude-code", useProxy: intent.useProxy });
  }
  return backend("init_provider_account_session", {
    id: intent.providerId,
    ...(intent.kind === "reauthorize_account" ? { accountId: intent.accountId, expectedVersion: intent.expectedVersion } : {}),
  });
}

/** Binding never carries a caller-selected account ID; the session owns its target. */
export async function bindClaudeAccountSession(context: OAuthSessionContext, label?: string): Promise<boolean> {
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

/** Match ordinary Claude login: support a code (including code#state) or callback URL. */
export function claudeOAuthCompletion(raw: string): { code?: string; callbackUrl?: string } {
  const value = raw.trim();
  if (!value) throw new Error("Paste the authorization code or full callback URL.");
  let url: URL;
  try { url = new URL(value); } catch { return { code: value }; }
  if (url.searchParams.has("error")) throw new Error("Authorization failed. Please start authorization again.");
  const code = url.searchParams.get("code");
  if (!code || !["http:", "https:"].includes(url.protocol)) throw new Error("No code parameter found in callback URL.");
  return { callbackUrl: value, code };
}
