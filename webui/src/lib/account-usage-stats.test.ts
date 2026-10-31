import { deepEqual, equal, ok } from "node:assert/strict";
import { test } from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useAccountUsageStats } from "./account-usage-stats";
import type { AccountUsageStats } from "./types";

function renderHook(client: QueryClient, id: string) {
  let result!: ReturnType<typeof useAccountUsageStats>;
  // Synchronous SSR probe only: capture the real hook result without a DOM test renderer.
  // eslint-disable-next-line react-hooks/globals
  function Probe() { result = useAccountUsageStats(id); return null; }
  renderToStaticMarkup(createElement(QueryClientProvider, { client }, createElement(Probe)));
  return result;
}

const zero = { total_tokens: 0, request_count: 0, equivalent_costs: [], unpriced_request_count: 0 };
const stats: AccountUsageStats[] = [
  { account_id: "b", all_time: zero, period: null, period_start: null, period_end: null },
  { account_id: "a", all_time: zero, period: zero, period_start: 100, period_end: 200 },
];

test("usage hook shares one provider query and maps backend cycles by stable account ID", () => {
  const client = new QueryClient();
  client.setQueryData(["provider-account-usage-stats", "provider/a"], stats);
  try {
    const result = renderHook(client, "provider/a");
    renderHook(client, "provider/a");
    equal(result.byAccount.size, 2);
    deepEqual(result.byAccount.get("a"), stats[1]);
    deepEqual(result.byAccount.get("b"), stats[0]);
    equal(result.byAccount.get("missing"), undefined);
    const queries = client.getQueryCache().findAll({ queryKey: ["provider-account-usage-stats"] });
    equal(queries.length, 1);
    deepEqual(queries[0].queryKey, ["provider-account-usage-stats", "provider/a"]);
    const options = queries[0].options as { staleTime?: number; refetchInterval?: number; refetchOnWindowFocus?: boolean };
    equal(options.staleTime, 30_000);
    equal(options.refetchInterval, 60_000);
    equal(options.refetchOnWindowFocus, false);
    // Another provider must not inherit this provider's rows.
    equal(renderHook(client, "provider/b").byAccount.size, 0);
  } finally { client.clear(); }
});

test("local calendar and reset boundaries do not create new usage queries or rewrite backend bounds", () => {
  const client = new QueryClient();
  const previousNow = Date.now;
  client.setQueryData(["provider-account-usage-stats", "provider/a"], stats);
  try {
    for (const now of [100, 199, 200, Date.parse("2027-01-01T00:00:00Z")]) {
      Date.now = () => now;
      deepEqual(renderHook(client, "provider/a").byAccount.get("a"), stats[1]);
    }
    equal(client.getQueryCache().findAll({ queryKey: ["provider-account-usage-stats"] }).length, 1);
  } finally { Date.now = previousNow; client.clear(); }
});

test("hook fetches all accounts once without a period parameter and forwards the cancellation signal", async () => {
  const previousFetch = globalThis.fetch;
  const previousStorage = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  Object.defineProperty(globalThis, "localStorage", { configurable: true, value: { getItem: () => null } });
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  let requests = 0;
  globalThis.fetch = async (url, init) => {
    requests++;
    equal(String(url), "/api/v1/providers/provider%2Fa/accounts/usage-stats");
    equal(init?.method, "GET");
    equal(init?.body, undefined);
    ok(init?.signal instanceof AbortSignal);
    return new Response(JSON.stringify({ data: stats }), { status: 200 });
  };
  try {
    const first = renderHook(client, "provider/a");
    const second = renderHook(client, "provider/a");
    const results = await Promise.all([first.query.refetch(), second.query.refetch()]);
    equal(requests, 1);
    for (const result of results) deepEqual(result.data, stats);
    deepEqual(renderHook(client, "provider/a").byAccount.get("b"), stats[0]);
  } finally {
    client.clear();
    globalThis.fetch = previousFetch;
    if (previousStorage) Object.defineProperty(globalThis, "localStorage", previousStorage);
    else Reflect.deleteProperty(globalThis, "localStorage");
  }
});
