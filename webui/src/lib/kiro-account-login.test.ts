import { deepEqual, equal, rejects } from "node:assert/strict";
import { test } from "node:test";
import { bindKiroAccountSession, initKiroAccountSession } from "./kiro-login";
import type { KiroLoginOptions } from "./types";
import type { OAuthSessionContext } from "./use-oauth-session";

async function withBackend(run: (requests: { url: string; body: unknown }[], respond: (value: unknown) => void) => Promise<void>) {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  const requests: { url: string; body: unknown }[] = [];
  let response: unknown = { session_id: "fixture" };
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

const methods: KiroLoginOptions[] = [
  { method: "google", region: " eu-west-1 " },
  { method: "github" },
  { method: "builder-id", idc_region: "us-east-1" },
  { method: "idc", idc_region: "eu-west-1", start_url: "https://example.awsapps.com/start" },
  { method: "import" },
];

test("all Kiro login methods initialize add and exact-account sessions through shared endpoints", async () => {
  await withBackend(async (requests) => {
    for (const options of methods) {
      await initKiroAccountSession({ kind: "add_account", providerId: "kiro/a" }, options);
      await initKiroAccountSession({ kind: "reauthorize_account", providerId: "kiro/a", accountId: "account/b", expectedVersion: 7 }, options);
      const kiro = options.region ? { ...options, region: options.region.trim() } : options;
      deepEqual(requests.splice(0), [
        { url: "/api/v1/providers/kiro%2Fa/accounts/auth-sessions", body: { account_id: null, expected_version: null, kiro } },
        { url: "/api/v1/providers/kiro%2Fa/accounts/auth-sessions", body: { account_id: "account/b", expected_version: 7, kiro } },
      ]);
    }
    await rejects(initKiroAccountSession({ kind: "add_account", providerId: "kiro" }, { method: "idc" }), /start URL and region/);
    equal(requests.length, 0);
  });
});

test("legacy login preserves proxy and Kiro options and rejects a newly configured pool", async () => {
  await withBackend(async (requests, respond) => {
    const intent = { kind: "reauthorize_legacy_provider" as const, providerId: "kiro", useProxy: true };
    respond({ provider_id: "kiro", config: null, accounts: [] });
    await initKiroAccountSession(intent, { method: "github" });
    deepEqual(requests.splice(0), [
      { url: "/api/v1/providers/kiro/accounts", body: undefined },
      { url: "/api/v1/oauth/sessions/init", body: { vendor: "kiro", use_proxy: true, kiro: { method: "github" } } },
    ]);
    respond({ provider_id: "kiro", config: {}, accounts: [] });
    await rejects(initKiroAccountSession(intent, { method: "import" }), /ACCOUNT_VERSION_CONFLICT/);
    equal(requests.length, 1);
  });
});

function context(overrides: Partial<OAuthSessionContext> = {}): OAuthSessionContext {
  return {
    intent: { kind: "reauthorize_account", providerId: "kiro", accountId: "chosen-account", expectedVersion: 7 },
    sessionId: "bound-session", signal: new AbortController().signal, isCurrent: () => true, ...overrides,
  };
}

test("binding consumes the backend-owned target without resending account IDs or credentials", async () => {
  await withBackend(async (requests) => {
    equal(await bindKiroAccountSession(context(), "Work"), true);
    deepEqual(requests, [{ url: "/api/v1/providers/kiro/accounts/bind", body: { session_id: "bound-session", label: "Work" } }]);
    requests.length = 0;
    equal(await bindKiroAccountSession(context({ isCurrent: () => false }), "Work"), false);
    equal(await bindKiroAccountSession(context({ intent: undefined }), "Work"), false);
    equal(requests.length, 0);
  });
});

test("legacy bind rechecks pool changes and cancellation after browser authorization", async () => {
  await withBackend(async (requests, respond) => {
    const legacy = context({ intent: { kind: "reauthorize_legacy_provider", providerId: "kiro", useProxy: false } });
    respond({ provider_id: "kiro", config: null, accounts: [{ provider_id: "kiro" }] });
    equal(await bindKiroAccountSession(legacy), true);
    deepEqual(requests.splice(0), [
      { url: "/api/v1/providers/kiro/accounts", body: undefined },
      { url: "/api/v1/providers/kiro/oauth/bind", body: { session_id: "bound-session" } },
    ]);
    respond({ provider_id: "kiro", config: {}, accounts: [] });
    await rejects(bindKiroAccountSession(legacy), /ACCOUNT_VERSION_CONFLICT/);
    equal(requests.splice(0).length, 1);
    respond({ provider_id: "kiro", config: null, accounts: [] });
    let checks = 0;
    equal(await bindKiroAccountSession({ ...legacy, isCurrent: () => ++checks === 1 }), false);
    equal(requests.length, 1);
  });
});
