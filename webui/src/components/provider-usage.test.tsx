import { doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { LocaleProvider } from "../lib/i18n";
import type { Provider, ProviderUsage } from "../lib/types";
import { ProviderUsageFooter } from "./provider-usage";

const now = Date.parse("2030-01-01T00:00:00Z");
const provider: Provider = {
  id: "usage", name: "Usage", vendor: "zai", protocol: "openai-compatible",
  base_url: "https://api.z.ai", use_proxy: false, fast_mode: false, is_enabled: true,
  created_at: "", updated_at: "",
};
const base: ProviderUsage = {
  provider_id: provider.id, kind: "glm_coding_plan", site: "global", tiers: [], balances: [], spends: [],
  is_available: true, queried_at: new Date(now).toISOString(),
};

function render(usage: ProviderUsage, error?: Error): string {
  const client = new QueryClient();
  const key = ["provider-usage", provider.id];
  client.setQueryData(key, usage);
  if (error) client.getQueryCache().find({ queryKey: key })!.setState({ status: "error", error });
  const originalNow = Date.now;
  Date.now = () => now;
  try {
    return renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      <ProviderUsageFooter provider={provider} />
    </LocaleProvider></QueryClientProvider>);
  } finally {
    Date.now = originalNow;
    client.clear();
  }
}

test("provider usage always renders every full tier and pace marker without disclosure", () => {
  const html = render({ ...base, tiers: [
    { name: "five_hour", used_percent: 27, resets_at: new Date(now + 2.5 * 3600_000).toISOString() },
    { name: "weekly_limit", used_percent: 19, resets_at: new Date(now + 3.5 * 86400_000).toISOString() },
    { name: "monthly", used_percent: 32, resets_at: new Date(now + 15 * 86400_000).toISOString() },
  ] });
  equal((html.match(/role="progressbar"/g) ?? []).length, 3);
  equal((html.match(/data-steady-pace-marker/g) ?? []).length, 3);
  match(html, /Steady pace: 50%/);
  match(html, /27%/);
  match(html, /19%/);
  match(html, /32%/);
  match(html, /Refresh usage/);
  doesNotMatch(html, /<details|<summary|Show details|Hide details|展开详情|收起详情/);
});

test("balance and spend details stay visible without an expand action", () => {
  const html = render({ ...base, kind: "deepseek_balance", balances: [
    { currency: "CNY", total: 56.14, granted: 1, topped_up: 55.14 },
  ], spends: [{ name: "today", currency: "CNY", amount: 2.5 }] });
  match(html, /56\.14/);
  match(html, /55\.14/);
  match(html, /2\.50/);
  doesNotMatch(html, /<details|<summary|Show details|Hide details/);
  match(render({ ...base, spends: [{ name: "today", currency: "USD", amount: 1.25 }] }), /1\.25/);
});

test("cached usage retains visible refresh failures, stale state and quota pause warning", () => {
  const html = render({ ...base, tiers: [{ name: "weekly_limit", used_percent: 100 }], scheduling: {
    status: "quota_exhausted", blocking_tiers: ["weekly_limit"], reason: "usage_limit",
  } }, new Error("Temporary status failure"));
  match(html, /Quota exhausted, routing paused/);
  match(html, /Usage query failed: Temporary status failure/);
  match(html, /Stale/);
  match(html, /aria-valuenow="100"/);
  doesNotMatch(html, /<details|<summary/);
});
