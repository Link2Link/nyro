import { deepEqual, equal, rejects, throws } from "node:assert/strict";
import { test } from "node:test";
import { backend } from "./backend";
import { bindClaudeAccountSession, claudeOAuthCompletion, initClaudeAccountSession } from "./claude-account-login";
import { isClaudeCodeOAuthProvider } from "./claude-account-pool";
import type { OAuthSessionContext } from "./use-oauth-session";

async function withBackend(run: (requests: { url: string; body: unknown }[], respond: (value: unknown) => void) => Promise<void>) {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  const requests: { url: string; body: unknown }[] = [];
  let response: unknown = { session_id: "fixture", auth_url: "https://claude.ai/oauth/authorize", completion_mode: "manual_exchange" };
  Object.defineProperty(globalThis, "localStorage", { configurable: true, value: { getItem: () => null } });
  globalThis.fetch = async (url, init) => {
    requests.push({ url: String(url), body: init?.body ? JSON.parse(String(init.body)) : undefined });
    return new Response(JSON.stringify({ data: response }), { status: 200 });
  };
  try { await run(requests, (value) => { response = value; }); }
  finally {
    globalThis.fetch = previousFetch;
    if (previousStorage) Object.defineProperty(globalThis, "localStorage", previousStorage);
    else Reflect.deleteProperty(globalThis, "localStorage");
  }
}

function context(overrides: Partial<OAuthSessionContext> = {}): OAuthSessionContext {
  return {
    intent: { kind: "reauthorize_account", providerId: "claude", accountId: "chosen-account", expectedVersion: 7 },
    sessionId: "bound-session", signal: new AbortController().signal, isCurrent: () => true, ...overrides,
  };
}

test("Claude pool gate requires Anthropic identity, claude-code channel and explicit OAuth", () => {
  const provider = { vendor: "anthropic", channel: "claude-code", auth_mode: "oauth" as const };
  equal(isClaudeCodeOAuthProvider(provider), true);
  equal(isClaudeCodeOAuthProvider({ ...provider, vendor: null, preset_key: "anthropic" }), true);
  equal(isClaudeCodeOAuthProvider({ ...provider, auth_mode: "apikey" }), false);
  equal(isClaudeCodeOAuthProvider({ ...provider, auth_mode: undefined }), false);
  equal(isClaudeCodeOAuthProvider({ ...provider, channel: "api" }), false);
  equal(isClaudeCodeOAuthProvider({ ...provider, vendor: "openai", preset_key: "anthropic" }), false);
  equal(isClaudeCodeOAuthProvider(null), false);
});

test("Claude init encodes add vs exact account intent before code exchange", async () => {
  await withBackend(async (requests) => {
    const session = await initClaudeAccountSession({ kind: "add_account", providerId: "claude/a" });
    equal(session.completion_mode, "manual_exchange");
    await initClaudeAccountSession({ kind: "reauthorize_account", providerId: "claude/a", accountId: "account/b", expectedVersion: 7 });
    deepEqual(requests.splice(0), [
      { url: "/api/v1/providers/claude%2Fa/accounts/auth-sessions", body: { account_id: null, expected_version: null } },
      { url: "/api/v1/providers/claude%2Fa/accounts/auth-sessions", body: { account_id: "account/b", expected_version: 7 } },
    ]);
    await backend("complete_oauth_session", { sessionId: "bound-session", ...claudeOAuthCompletion(" code#state ") });
    equal(await bindClaudeAccountSession(context(), "Work"), true);
    deepEqual(requests, [
      { url: "/api/v1/oauth/sessions/bound-session/complete", body: { code: "code#state", metadata: {} } },
      { url: "/api/v1/providers/claude/accounts/bind", body: { session_id: "bound-session", label: "Work" } },
    ]);
  });
});

test("legacy Claude init preserves driver and proxy while rejecting ambiguous pools", async () => {
  await withBackend(async (requests, respond) => {
    const intent = { kind: "reauthorize_legacy_provider" as const, providerId: "claude", useProxy: true };
    respond({ provider_id: "claude", config: null, accounts: [] });
    await initClaudeAccountSession(intent);
    deepEqual(requests.splice(0), [
      { url: "/api/v1/providers/claude/accounts", body: undefined },
      { url: "/api/v1/oauth/sessions/init", body: { vendor: "claude-code", use_proxy: true } },
    ]);
    for (const value of [{ provider_id: "claude", config: {}, accounts: [] }, { provider_id: "claude", config: null, accounts: [{ provider_id: "claude" }, { provider_id: "claude" }] }]) {
      respond(value);
      await rejects(initClaudeAccountSession(intent), /ACCOUNT_VERSION_CONFLICT/);
      equal(requests.splice(0).length, 1);
    }
  });
});

test("cancelled or missing-intent sessions never bind; legacy bind revalidates after browser login", async () => {
  await withBackend(async (requests, respond) => {
    equal(await bindClaudeAccountSession(context({ isCurrent: () => false })), false);
    equal(await bindClaudeAccountSession(context({ intent: undefined })), false);
    equal(requests.length, 0);
    const legacy = context({ intent: { kind: "reauthorize_legacy_provider", providerId: "claude", useProxy: false } });
    respond({ provider_id: "claude", config: null, accounts: [{ provider_id: "claude" }] });
    equal(await bindClaudeAccountSession(legacy), true);
    deepEqual(requests.splice(0), [
      { url: "/api/v1/providers/claude/accounts", body: undefined },
      { url: "/api/v1/providers/claude/oauth/bind", body: { session_id: "bound-session" } },
    ]);
    respond({ provider_id: "claude", config: {}, accounts: [] });
    await rejects(bindClaudeAccountSession(legacy), /ACCOUNT_VERSION_CONFLICT/);
    equal(requests.splice(0).length, 1);
    respond({ provider_id: "claude", config: null, accounts: [] });
    let checks = 0;
    equal(await bindClaudeAccountSession({ ...legacy, isCurrent: () => ++checks === 1 }), false);
    equal(requests.length, 1);
  });
});

test("Claude callback completion preserves code/state and rejects empty, failed or code-less URLs", () => {
  deepEqual(claudeOAuthCompletion(" code#state "), { code: "code#state" });
  const callbackUrl = "https://console.anthropic.com/oauth/code/callback?code=abc&state=xyz";
  deepEqual(claudeOAuthCompletion(callbackUrl), { callbackUrl, code: "abc" });
  for (const value of ["", "https://example.com/no-code", "https://example.com/?error=denied&code=ignored", "javascript:alert(1)"]) throws(() => claudeOAuthCompletion(value));
});
