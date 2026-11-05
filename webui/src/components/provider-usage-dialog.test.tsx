import { deepEqual, doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import type { ReactElement } from "react";
import type { QuotaBucket, QuotaBucketFilter } from "../lib/types";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { LocaleProvider } from "../lib/i18n";
import { ProviderUsageDialog, QuotaBucketFilters } from "./provider-usage-dialog";

const bucket: QuotaBucket = { id: "bucket", scope_id: "scope", label: "Flash", models: ["gemini-3.8-flash"],
  binding: { status: "resolved", source: "configured", revision: 2 }, tiers: [], constraint_ids: [], source: "enforced_summary",
  queried_at: "2030-01-01T00:00:00Z", stale: false, scheduling: { status: "eligible", blocking_tiers: [] } };
const buckets = [bucket, { ...bucket, label: "Argon", scope_id: "scope-b" },
  ...["shared_constraint", "window_constraint"].map((status) => ({ ...bucket, id: status, label: status, binding: { ...bucket.binding, status } }))];

test("bucket filters expose All / exact scoped buckets / Unassigned without shared capacity entries", () => {
  const html = renderToStaticMarkup(<QuotaBucketFilters buckets={buckets} value={{ quota_bucket: "bucket", quota_scope: "scope-b" }} onChange={() => {}} />);
  match(html, /Filter by quota bucket/);
  equal((html.match(/aria-pressed="true"/g) ?? []).length, 1);
  match(html, /aria-pressed="true"[^>]*>Argon</);
  for (const name of ["All", "Flash", "Argon", "Unassigned"]) match(html, new RegExp(`>${name}<`));
  doesNotMatch(html, /shared_constraint|window_constraint/);
  const zh = renderToStaticMarkup(<QuotaBucketFilters buckets={buckets} value={{}} onChange={() => {}} isZh />);
  match(zh, /按配额桶筛选/);
  match(zh, />全部</);
  match(zh, />未归属</);
});

test("Google display families never become synthetic retained-log filter actions", () => {
  const family: QuotaBucket = { ...bucket, id: "display-flash", label: "gemini-3.8-flash",
    source: "google_family_display_v1", binding: { ...bucket.binding, source: "google_family_display_v1" } };
  const selections: QuotaBucketFilter[] = [];
  const element = QuotaBucketFilters({ buckets: [...buckets, family], value: {}, onChange: (value) => selections.push(value) });
  const buttons = element.props.children as ReactElement<{ onClick: () => void }>[];
  for (const button of buttons) button.props.onClick();
  equal(selections.some((filter) => filter.quota_bucket === family.id), false);
  const html = renderToStaticMarkup(<QuotaBucketFilters buckets={[family]} value={{ quota_bucket: family.id, quota_scope: family.scope_id }} onChange={() => {}} />);
  doesNotMatch(html, /gemini-3\.8-flash|display-flash|Retained bucket/);
  match(html, />All</);
  match(html, />Unassigned</);
});

test("dialog overview cache identities include the attribution filter and retain the real provider ID", () => {
  const client = new QueryClient();
  const filter = { quota_bucket: "bucket", quota_scope: "scope-b" };
  try {
    renderToStaticMarkup(<QueryClientProvider client={client}><LocaleProvider>
      <ProviderUsageDialog open initialProviderId="real-provider" initialRange={{ kind: "hours", hours: 24 }}
        initialQuotaFilter={filter} quotaBuckets={buckets} onOpenChange={() => {}} />
    </LocaleProvider></QueryClientProvider>);
    const detail = client.getQueryCache().find({ queryKey: ["provider-usage-detail"], exact: false });
    deepEqual(detail?.queryKey, ["provider-usage-detail", "real-provider", "hours:24", filter]);
    equal(client.getQueryCache().find({ queryKey: ["provider-usage", "real-provider"] }) != null, true);
  } finally { client.clear(); }
});

test("filter actions preserve exact bucket scope identities, including same IDs in different scopes", () => {
  const selections: QuotaBucketFilter[] = [];
  const element = QuotaBucketFilters({ buckets, value: {}, onChange: (filter) => selections.push(filter) });
  const buttons = element.props.children as ReactElement<{ onClick: () => void }>[];
  for (const button of buttons) button.props.onClick();
  deepEqual(selections, [{}, { quota_bucket: "bucket", quota_scope: "scope" },
    { quota_bucket: "bucket", quota_scope: "scope-b" }, { quota_bucket: "unassigned" }]);
});
