import type { ProviderUsage, QuotaBucket } from "./types";

const GOOGLE_FAMILY_DISPLAY_SOURCE = "google_family_display_v1";
export const isPrimaryQuotaBucket = (bucket: QuotaBucket) => !["shared_constraint", "window_constraint"].includes(bucket.binding.status);
export const isGoogleFamilyDisplayBucket = (bucket: QuotaBucket) =>
  bucket.source === GOOGLE_FAMILY_DISPLAY_SOURCE || bucket.binding.source === GOOGLE_FAMILY_DISPLAY_SOURCE;

/** Nyro route buckets carry admission decisions, not observed quota or local accounting. */
export const isForwardedRouteBucket = (bucket: QuotaBucket) =>
  bucket.source === "forwarded_route" || bucket.binding.source === "forwarded_route_v1";

/** Select server-projected cards only; a relay displays the remote report instead of admission buckets. */
export const providerQuotaDisplayBuckets = (usage: ProviderUsage) => usage.upstream ? [] : (usage.quota_buckets ?? [])
  .filter((bucket) => !isForwardedRouteBucket(bucket) && isPrimaryQuotaBucket(bucket)
    && (usage.kind !== "google_subscription" || isGoogleFamilyDisplayBucket(bucket)));

/** Missing fields keep legacy servers compatible; an explicit empty Google snapshot is unknown, not a parent quota. */
export const hasStructuredQuotaBuckets = (usage?: ProviderUsage) => !usage?.upstream
  && (Boolean(usage?.quota_buckets?.some((bucket) => !isForwardedRouteBucket(bucket)))
    || (usage?.kind === "google_subscription" && usage.quota_buckets != null));
