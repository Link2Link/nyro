import type { ProviderUsage, QuotaBucket } from "./types";

const GOOGLE_FAMILY_DISPLAY_SOURCE = "google_family_display_v1";
export const isPrimaryQuotaBucket = (bucket: QuotaBucket) => !["shared_constraint", "window_constraint"].includes(bucket.binding.status);
export const isGoogleFamilyDisplayBucket = (bucket: QuotaBucket) =>
  bucket.source === GOOGLE_FAMILY_DISPLAY_SOURCE || bucket.binding.source === GOOGLE_FAMILY_DISPLAY_SOURCE;

/** Select server-projected cards only; raw observations never establish a family in the client. */
export const providerQuotaDisplayBuckets = (usage: ProviderUsage) => (usage.quota_buckets ?? [])
  .filter((bucket) => isPrimaryQuotaBucket(bucket) && (usage.kind !== "google_subscription" || isGoogleFamilyDisplayBucket(bucket)));

/** Missing fields keep legacy servers compatible; an explicit empty Google snapshot is unknown, not a parent quota. */
export const hasStructuredQuotaBuckets = (usage?: ProviderUsage) => Boolean(usage?.quota_buckets?.length)
  || (usage?.kind === "google_subscription" && usage.quota_buckets != null);
