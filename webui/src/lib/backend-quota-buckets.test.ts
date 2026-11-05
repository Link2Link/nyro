import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { backend } from "./backend";
import type { LogQuery, QuotaBucketConfig } from "./types";

test("Nyro HTTP explicitly selects structured real providers, preserving exact-provider usage", async () => {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  Object.defineProperty(globalThis, "localStorage", { configurable: true, value: { getItem: () => null } });
  const requests: string[] = [];
  globalThis.fetch = async (url) => { requests.push(String(url)); return new Response(JSON.stringify({ data: [] }), { status: 200 }); };
  try {
    await backend("list_provider_usage");
    await backend("get_provider_usage", { id: "real-provider" });
    deepEqual(requests, ["/api/v1/providers/usage?view=providers", "/api/v1/providers/real-provider/usage"]);
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage) Object.defineProperty(globalThis, "localStorage", previousStorage);
    else Reflect.deleteProperty(globalThis, "localStorage");
  }
});

test("detail and paginated log queries send the same bucket/scope and compose exact-model filters", async () => {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  Object.defineProperty(globalThis, "localStorage", { configurable: true, value: { getItem: () => null } });
  const requests: { url: string; method?: string; body: unknown }[] = [];
  globalThis.fetch = async (url, init) => {
    requests.push({ url: String(url), method: init?.method, body: init?.body ? JSON.parse(String(init.body)) : undefined });
    return new Response(JSON.stringify({ data: {} }), { status: 200 });
  };
  try {
    const query: LogQuery = { provider: "real/provider", quota_bucket: "bucket/id", quota_scope: "scope:id",
      upstream_model: "gemini-4-argon-high", limit: 10, offset: 20, after: 10, before: 30, status_min: 400 };
    await backend("get_provider_usage_detail", { id: query.provider, startAt: 10, endAt: 30,
      quotaBucket: query.quota_bucket, quotaScope: query.quota_scope });
    await backend("query_logs", { query });
    const detail = new URL(requests[0].url, "http://localhost");
    const logs = new URL(requests[1].url, "http://localhost");
    equal(detail.pathname, "/api/v1/stats/providers/real%2Fprovider");
    for (const key of ["quota_bucket", "quota_scope"]) equal(detail.searchParams.get(key), logs.searchParams.get(key));
    equal(detail.searchParams.get("start_at"), "10");
    equal(logs.searchParams.get("upstream_model"), query.upstream_model);
    equal(logs.searchParams.get("offset"), "20");
    await backend("get_provider_usage_detail", { id: "provider", hours: 24, quotaBucket: "unassigned" });
    const unassigned = new URL(requests[2].url, "http://localhost");
    equal(unassigned.searchParams.get("quota_bucket"), "unassigned");
    equal(unassigned.searchParams.has("quota_scope"), false);
    await backend("get_provider_usage_detail", { id: "provider", hours: 24 });
    equal(requests[3].url, "/api/v1/stats/providers/provider?hours=24");
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage) Object.defineProperty(globalThis, "localStorage", previousStorage);
    else Reflect.deleteProperty(globalThis, "localStorage");
  }
});

test("quota binding configuration is provider-scoped and sends the versioned config wrapper", async () => {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  Object.defineProperty(globalThis, "localStorage", { configurable: true, value: { getItem: () => null } });
  const config: QuotaBucketConfig = { revision: 3, bindings: [{ id: "immutable", scope_id: "scope", label: "Gemini Flash",
    models: ["gemini-3.8-flash-high"], constraint_ids: ["window-five-hour", "window-weekly", "shared-global"] }] };
  const requests: { url: string; method?: string; body: unknown }[] = [];
  globalThis.fetch = async (url, init) => {
    requests.push({ url: String(url), method: init?.method, body: init?.body ? JSON.parse(String(init.body)) : undefined });
    return new Response(JSON.stringify({ data: config }), { status: 200 });
  };
  try {
    deepEqual(await backend("get_provider_quota_buckets_config", { id: "real/provider" }), config);
    deepEqual(await backend("set_provider_quota_buckets_config", { id: "real/provider", config }), config);
    deepEqual(requests, [
      { url: "/api/v1/providers/real%2Fprovider/quota-buckets", method: "GET", body: undefined },
      { url: "/api/v1/providers/real%2Fprovider/quota-buckets", method: "PUT", body: { config } },
    ]);
  } finally {
    globalThis.fetch = previousFetch;
    if (previousStorage) Object.defineProperty(globalThis, "localStorage", previousStorage);
    else Reflect.deleteProperty(globalThis, "localStorage");
  }
});
