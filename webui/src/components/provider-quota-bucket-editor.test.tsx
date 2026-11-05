import { doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { renderToStaticMarkup } from "react-dom/server";
import type { ManualQuotaBucketBinding, QuotaBucket } from "../lib/types";
import { QuotaBindingFields } from "./provider-quota-bucket-editor";

const resource: QuotaBucket = { id: "observed-resource", scope_id: "scope-a", label: "Observed five-hour allowance", models: [],
  binding: { status: "window_constraint", source: "official", revision: 0 }, tiers: [{ name: "five_hour", used_percent: 17 }],
  constraint_ids: [], source: "enforced_summary", queried_at: "2030-01-01T00:00:00Z", stale: false,
  scheduling: { status: "eligible", blocking_tiers: [] } };
const binding: ManualQuotaBucketBinding = { id: "stable-local-id", scope_id: "scope-a", label: "Gemini Flash",
  models: ["gemini-3.8-flash-high", "gemini-3.8-flash-low"], constraint_ids: [resource.id] };
const render = (isZh = false, extra: QuotaBucket[] = []) => renderToStaticMarkup(<QuotaBindingFields binding={binding}
  resources={[resource, ...extra]} existing isZh={isZh} onChange={() => {}} onRemove={() => {}} />);

test("explicit binding editor shows exact models and observed resources without matching names or percentages", () => {
  const html = render(false, [{ ...resource, id: "other-owner", scope_id: "scope-b", label: "Hidden resource" },
    { ...resource, id: "derived-configured", label: "Derived configured bucket", binding: { ...resource.binding, source: "configured" } }]);
  match(html, /Stable binding ID \(immutable\)/);
  match(html, /readOnly=""[^]*value="stable-local-id"|value="stable-local-id"[^]*readOnly=""/);
  match(html, /Display label \(English default\)/);
  match(html, /gemini-3\.8-flash-high\ngemini-3\.8-flash-low/);
  match(html, /observed-resource/);
  equal((html.match(/type="checkbox"/g) ?? []).length, 1);
  doesNotMatch(html, /Hidden resource|Derived configured bucket|17%/);
});

test("binding editor explicitly localizes guidance and preserves absent historical references for validation", () => {
  match(render(true), /精确上游模型 ID/);
  const html = renderToStaticMarkup(<QuotaBindingFields binding={{ ...binding, constraint_ids: ["retired-resource"] }}
    resources={[resource]} onChange={() => {}} onRemove={() => {}} />);
  match(html, /Resource absent from current snapshot; backend will validate on save/);
  match(html, /retired-resource/);
});

test("Google display families cannot become binding resources while raw windows and shared constraints remain available", () => {
  const family: QuotaBucket = { ...resource, id: "synthetic-family", label: "Synthetic family", models: ["gemini-3.8-flash"],
    source: "google_family_display_v1", binding: { ...resource.binding, status: "resolved", source: "google_family_display_v1" } };
  const html = render(false, [family,
    { ...family, id: "source-marker-only", source: "google_family_display_v1", binding: { ...resource.binding } },
    { ...family, id: "binding-marker-only", source: "enforced_summary" },
    { ...family, id: "family-other-scope", scope_id: "synthetic-only-scope" },
    { ...resource, id: "raw-weekly", label: "Raw weekly window", tiers: [{ name: "weekly_limit", used_percent: 20 }] },
    { ...resource, id: "raw-shared", label: "Raw shared constraint", binding: { ...resource.binding, status: "shared_constraint" } },
    { ...resource, scope_id: "scope-b", label: "Other owner same raw ID" }]);
  equal((html.match(/type="checkbox"/g) ?? []).length, 3);
  for (const value of ["observed-resource", "raw-weekly", "raw-shared", "Weekly"]) match(html, new RegExp(value));
  doesNotMatch(html, /synthetic-family|Synthetic family|source-marker-only|binding-marker-only|synthetic-only-scope|Other owner same raw ID/);
});
