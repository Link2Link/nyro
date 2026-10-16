import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { backend } from "./backend";

test("HTTP OAuth initialization preserves typed Kiro options and legacy requests", async () => {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  const requests: { url: string; body: unknown }[] = [];
  Object.defineProperty(globalThis, "localStorage", { configurable: true, value: { getItem: () => null } });
  globalThis.fetch = async (url, init) => {
    requests.push({ url: String(url), body: JSON.parse(String(init?.body)) });
    return new Response(JSON.stringify({ data: { session_id: "fixture" } }), { status: 200 });
  };
  try {
    const kiro = { method: "idc", region: "eu-west-1", idc_region: "us-east-1", start_url: "https://example.awsapps.com/start" };
    await backend("init_oauth_session", { vendor: "kiro", useProxy: true, kiro });
    await backend("init_oauth_session", { vendor: "kiro", useProxy: false });
    equal(requests[0].url, "/api/v1/oauth/sessions/init");
    deepEqual(requests[0].body, { vendor: "kiro", use_proxy: true, kiro });
    deepEqual(requests[1].body, { vendor: "kiro", use_proxy: false });
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage) Object.defineProperty(globalThis, "localStorage", previousStorage);
    else Reflect.deleteProperty(globalThis, "localStorage");
  }
});
