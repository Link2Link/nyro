import { useMemo } from "react";
import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { backend } from "./backend";
import { emptyRatingProfileInput, hasModelRating, isProviderModelRatingProfile, providerModelKey, readProviderModelRatingProfiles, type RatingLoadState } from "./model-ratings";
import { EFFORT_TIERS, type ProviderModelRatingProfile, type SetProviderModelRatingProfile } from "./types";

export const MODEL_RATINGS_QUERY_KEY = ["provider-model-rating-profiles"] as const;

export function invalidateModelRatings(client: QueryClient) {
  return Promise.all([
    client.invalidateQueries({ queryKey: MODEL_RATINGS_QUERY_KEY }),
    client.invalidateQueries({ queryKey: ["model-performance"] }),
  ]);
}

/** The same unfiltered profile cache is shared by every rating surface; no per-row requests. */
export function useModelRatings() {
  const query = useQuery({
    queryKey: MODEL_RATINGS_QUERY_KEY,
    queryFn: async () => readProviderModelRatingProfiles(await backend<unknown>("list_provider_model_rating_profiles")),
    retry: false,
    staleTime: 0,
    refetchOnWindowFocus: "always",
    refetchOnMount: "always",
    refetchOnReconnect: "always",
  });
  const index = useMemo(() => new Map((query.data ?? []).map((rating) => [
    providerModelKey(rating.provider_id, rating.upstream_model), rating,
  ])), [query.data]);
  const loadState: RatingLoadState = query.isError ? "error" : query.isSuccess ? "ready" : "loading";
  return { ...query, index, loadState };
}

interface RatingIdentity {
  providerId: string;
  model: string;
}

async function setRatingProfile({ providerId, model }: RatingIdentity, input: SetProviderModelRatingProfile) {
  for (const score of [input.common, ...EFFORT_TIERS.map((tier) => input.overrides[tier])]) {
    if (score !== null && (typeof score !== "number" || !Number.isInteger(score) || score < 0 || score > 100)) {
      throw new Error("Every enabled score must be an integer from 0 to 100.");
    }
  }
  const saved = await backend<unknown>("set_provider_model_rating_profile", { providerId, model, input });
  if (!isProviderModelRatingProfile(saved) || saved.provider_id !== providerId || saved.upstream_model !== model
    || (saved.common?.score ?? null) !== input.common
    || EFFORT_TIERS.some((tier) => (saved.overrides[tier]?.score ?? null) !== input.overrides[tier])) {
    throw new Error("Invalid model rating profile response. Please retry loading ratings.");
  }
  return saved;
}

export function useModelRatingMutations() {
  const client = useQueryClient();
  async function syncSavedRating(identity: RatingIdentity, saved: ProviderModelRatingProfile) {
    await client.cancelQueries({ queryKey: MODEL_RATINGS_QUERY_KEY });
    const key = providerModelKey(identity.providerId, identity.model);
    client.setQueryData<ProviderModelRatingProfile[]>(MODEL_RATINGS_QUERY_KEY, (previous) => {
      // Never invent a successful full list from a single-profile mutation.
      if (!previous) return previous;
      const next = previous.filter((rating) => providerModelKey(rating.provider_id, rating.upstream_model) !== key);
      return hasModelRating(saved) ? [...next, saved] : next;
    });
    await invalidateModelRatings(client);
  }
  const save = useMutation({
    mutationFn: async ({ input, ...identity }: RatingIdentity & { input: SetProviderModelRatingProfile }) => setRatingProfile(identity, input),
    onSuccess: (saved, identity) => syncSavedRating(identity, saved),
  });
  const clear = useMutation({
    mutationFn: async (identity: RatingIdentity) => setRatingProfile(identity, emptyRatingProfileInput()),
    onSuccess: (saved, identity) => syncSavedRating(identity, saved),
  });
  return { save, clear };
}
