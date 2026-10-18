import { deepEqual, equal, rejects } from "node:assert/strict";
import { test } from "node:test";
import { backend, BackendHTTPError } from "./backend";
import { newDevinPoolConfig } from "./devin-account-pool";

test("account HTTP mappings preserve provider/account addressing, intent and CAS versions", async () => {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  const requests: { url: string; method: string | undefined; body: unknown }[] = [];
  Object.defineProperty(globalThis, "localStorage", { configurable: true, value: { getItem: () => null } });
  globalThis.fetch = async (url, init) => {
    requests.push({ url: String(url), method: init?.method, body: init?.body ? JSON.parse(String(init.body)) : undefined });
    return new Response(JSON.stringify({ data: null }), { status: 200 });
  };
  try {
    const id = "provider/a";
    const accountId = "account/b";
    const config = newDevinPoolConfig();
    equal("plan" in config, false);
    await backend("create_devin_provider_with_pool", { sessionId: "create-session", input: { name: "Devin" }, config });
    deepEqual(requests.shift(), { method: "POST", url: "/api/v1/providers/devin", body: { session_id: "create-session", input: { name: "Devin" }, config } });
    await backend("create_devin_provider_with_pool", { sessionId: "auto-session", input: { name: "Automatic Devin" } });
    deepEqual(requests.shift(), { method: "POST", url: "/api/v1/providers/devin", body: { session_id: "auto-session", input: { name: "Automatic Devin" } } });
    equal(await backend("get_provider_account_pool", { id }), null);
    await backend("configure_provider_account_pool", { id, config });
    await backend("init_provider_account_session", { id });
    await backend("init_provider_account_session", { id, accountId, expectedVersion: 7 });
    await backend("bind_provider_account", { id, sessionId: "session", label: "Work" });
    await backend("update_provider_account", { id, accountId, expectedVersion: 7, update: { is_enabled: false } });
    await backend("disconnect_provider_account", { id, accountId, expectedVersion: 8 });
    await backend("refresh_provider_account_usage", { id, accountId });
    await backend("refresh_provider_account_pool_usage", { id });
    const base = "/api/v1/providers/provider%2Fa";
    deepEqual(requests, [
      { method: "GET", url: `${base}/accounts`, body: undefined },
      { method: "PUT", url: `${base}/account-pool`, body: config },
      { method: "POST", url: `${base}/accounts/auth-sessions`, body: { account_id: null, expected_version: null } },
      { method: "POST", url: `${base}/accounts/auth-sessions`, body: { account_id: accountId, expected_version: 7 } },
      { method: "POST", url: `${base}/accounts/bind`, body: { session_id: "session", label: "Work" } },
      { method: "PATCH", url: `${base}/accounts/account%2Fb`, body: { expected_version: 7, update: { is_enabled: false } } },
      { method: "DELETE", url: `${base}/accounts/account%2Fb`, body: { expected_version: 8 } },
      { method: "POST", url: `${base}/accounts/account%2Fb/usage/refresh`, body: undefined },
      { method: "POST", url: `${base}/account-pool/usage/refresh`, body: undefined },
    ]);
    requests.length = 0;
    await backend("init_oauth_session", { vendor: "devin", useProxy: true });
    await backend("bind_provider_oauth", { id: "legacy-provider", sessionId: "legacy-session" });
    deepEqual(requests, [
      { method: "POST", url: "/api/v1/oauth/sessions/init", body: { vendor: "devin", use_proxy: true } },
      { method: "POST", url: "/api/v1/providers/legacy-provider/oauth/bind", body: { session_id: "legacy-session" } },
    ]);
    globalThis.fetch = async () => new Response(JSON.stringify({ error: "ACCOUNT_VERSION_CONFLICT" }), { status: 409 });
    await rejects(backend("disconnect_provider_account", { id, accountId, expectedVersion: 7 }),
      (error: unknown) => error instanceof BackendHTTPError && error.status === 409);
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage) Object.defineProperty(globalThis, "localStorage", previousStorage);
    else Reflect.deleteProperty(globalThis, "localStorage");
  }
});
