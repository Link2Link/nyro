import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import { hasStructuredQuotaBuckets, isForwardedRouteBucket, providerQuotaDisplayBuckets } from "./quota-bucket-display";
import type { ProviderUsage, QuotaBucket } from "./types";

const route: QuotaBucket = {
  id: "route", scope_id: "local-forwarded-scope", label: "Remote route", models: ["remote-model"],
  binding: { status: "resolved", source: "forwarded_route_v1", revision: 1 },
  source: "forwarded_route", tiers: [], constraint_ids: [], queried_at: "2030-01-01T00:00:00Z", stale: false,
  scheduling: { status: "eligible", blocking_tiers: [] },
};
const quota: QuotaBucket = { ...route, id: "actual-quota", label: "Actual quota", source: "enforced_summary",
  binding: { ...route.binding, source: "configured" }, tiers: [{ name: "five_hour", used_percent: 41 }] };
const usage = (over: Partial<ProviderUsage>): ProviderUsage => ({
  provider_id: "local", kind: "nyro_gateway", site: "global", tiers: [], queried_at: route.queried_at, ...over,
});

test("forwarded route identity distinguishes admission metadata from actual quota buckets", () => {
  for (const bucket of [route, { ...route, binding: { ...route.binding, source: "adapter" } },
    { ...route, source: "adapter" }]) equal(isForwardedRouteBucket(bucket), true);
  equal(isForwardedRouteBucket(quota), false);
  equal(hasStructuredQuotaBuckets(usage({ quota_buckets: [route] })), false);
  equal(hasStructuredQuotaBuckets(usage({ quota_buckets: [quota] })), true);
  deepEqual(providerQuotaDisplayBuckets(usage({ quota_buckets: [route, quota] })), [quota]);
});

test("a forwarded upstream report wins presentation even when local quota buckets are nonempty", () => {
  equal(hasStructuredQuotaBuckets(usage({ upstream: { providers: [] }, quota_buckets: [route, quota] })), false);
  equal(hasStructuredQuotaBuckets(usage({ upstream: { providers: [] }, quota_buckets: [] })), false);
});

test("ordinary structured quotas and explicit empty Google snapshots preserve compatibility", () => {
  equal(hasStructuredQuotaBuckets(undefined), false);
  equal(hasStructuredQuotaBuckets(usage({ kind: "google_subscription", quota_buckets: undefined })), false);
  equal(hasStructuredQuotaBuckets(usage({ kind: "google_subscription", quota_buckets: [] })), true);
  equal(hasStructuredQuotaBuckets(usage({ kind: "other_subscription", quota_buckets: [] })), false);
  equal(hasStructuredQuotaBuckets(usage({ kind: "other_subscription", quota_buckets: [quota] })), true);
  deepEqual(providerQuotaDisplayBuckets(usage({ kind: "other_subscription", quota_buckets: [quota] })), [quota]);
});
