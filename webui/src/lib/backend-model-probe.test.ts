import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { backend } from "./backend";
import type { ModelProbeOutcome } from "./types";

test("HTTP model probe sends optional account_id and preserves confirmed account metadata", async () => {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  const requests: { url: string; method?: string; body: unknown }[] = [];
  const outcome: ModelProbeOutcome = {
    meta: { protocol: "test", base_url: "https://example.test", account_id: "selected", account_label: "Work" },
    results: [{ model: "model", success: false, latency_ms: 3, protocol: "test", error: "denied" }],
  };
  Object.defineProperty(globalThis, "localStorage", { configurable: true, value: { getItem: () => null } });
  globalThis.fetch = async (url, init) => {
    requests.push({ url: String(url), method: init?.method, body: init?.body ? JSON.parse(String(init.body)) : undefined });
    return new Response(JSON.stringify({ data: outcome }), { status: 200 });
  };
  try {
    deepEqual(await backend<ModelProbeOutcome>("probe_provider_models", {
      id: "provider", models: ["model"], prompt: "hello", accountId: "selected",
    }), outcome);
    await backend("probe_provider_models", { id: "provider", models: ["model"], prompt: " " });
    await backend("probe_provider_models", { id: "provider" });
    await backend("probe_provider_models", { id: "provider", accountId: "selected" });
    // Never silently remove invalid pinned ids: the server must reject them.
    for (const accountId of ["", "   ", null, 42, undefined]) {
      await backend("probe_provider_models", { id: "provider", accountId });
    }
    deepEqual(requests.map((request) => request.body), [
      { models: ["model"], prompt: "hello", account_id: "selected" },
      { models: ["model"] }, undefined, { account_id: "selected" },
      { account_id: "" }, { account_id: "   " }, { account_id: null }, { account_id: 42 }, undefined,
    ]);
    for (const request of requests) {
      equal(request.url, "/api/v1/providers/provider/probe-models");
      equal(request.method, "POST");
    }
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage) Object.defineProperty(globalThis, "localStorage", previousStorage);
    else Reflect.deleteProperty(globalThis, "localStorage");
  }
});
