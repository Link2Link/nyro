import { useMemo } from "react";
import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { backend } from "./backend";
import { buildAaLookupIndex } from "./aa-rating-match";
import { canonicalModelPrefix, isModelRatingEntry, readModelRatings, type RatingLoadState } from "./model-ratings";
import type { AaSyncConfigView, AaSyncOverview, ModelRatingEntry, UpdateAaSyncConfig } from "./types";

export const MODEL_RATINGS_QUERY_KEY = ["model-ratings"] as const;
export const AA_RATING_SYNC_QUERY_KEY = ["aa-rating-sync"] as const;

export function invalidateModelRatings(client: QueryClient) {
  return Promise.all([
    client.invalidateQueries({ queryKey: MODEL_RATINGS_QUERY_KEY }),
    client.invalidateQueries({ queryKey: ["model-performance"] }),
  ]);
}

export function invalidateAaRatingSync(client: QueryClient) {
  return Promise.all([
    client.invalidateQueries({ queryKey: AA_RATING_SYNC_QUERY_KEY }),
    client.invalidateQueries({ queryKey: ["model-performance"] }),
  ]);
}

/** The same unfiltered list cache is shared by every rating surface; no per-row requests. */
export function useModelRatings() {
  const query = useQuery({
    queryKey: MODEL_RATINGS_QUERY_KEY,
    queryFn: async () => readModelRatings(await backend<unknown>("list_model_ratings")),
    retry: false,
    staleTime: 0,
    refetchOnWindowFocus: "always",
    refetchOnMount: "always",
    refetchOnReconnect: "always",
  });
  const loadState: RatingLoadState = query.isError ? "error" : query.isSuccess ? "ready" : "loading";
  return { ...query, loadState };
}

export function useAaRatingSync() {
  const query = useQuery<AaSyncOverview>({
    queryKey: AA_RATING_SYNC_QUERY_KEY,
    queryFn: () => backend<AaSyncOverview>("get_aa_rating_sync"),
    retry: false,
    staleTime: 30_000,
    refetchOnWindowFocus: false,
  });
  const index = useMemo(
    () => buildAaLookupIndex(query.data?.models ?? []),
    [query.data?.models],
  );
  return { ...query, index };
}

export function useAaRatingSyncMutations() {
  const client = useQueryClient();
  const updateConfig = useMutation({
    mutationFn: (input: UpdateAaSyncConfig) =>
      backend<AaSyncConfigView>("update_aa_rating_sync_config", { input }),
    onSuccess: () => invalidateAaRatingSync(client),
  });
  const triggerSync = useMutation({
    mutationFn: () => backend<AaSyncOverview>("trigger_aa_rating_sync"),
    onSuccess: (overview) => {
      client.setQueryData(AA_RATING_SYNC_QUERY_KEY, overview);
      return invalidateAaRatingSync(client);
    },
  });
  const setAlias = useMutation({
    mutationFn: ({ modelPrefix, aaCanonicalKey }: { modelPrefix: string; aaCanonicalKey: string }) =>
      backend("set_aa_model_alias", { modelPrefix, input: { aa_canonical_key: aaCanonicalKey } }),
    onSuccess: () => invalidateAaRatingSync(client),
  });
  const deleteAlias = useMutation({
    mutationFn: ({ modelPrefix }: { modelPrefix: string }) =>
      backend("delete_aa_model_alias", { modelPrefix }),
    onSuccess: () => invalidateAaRatingSync(client),
  });
  return { updateConfig, triggerSync, setAlias, deleteAlias };
}

interface RatingIdentity {
  modelPrefix: string;
}

export function useModelRatingMutations() {
  const client = useQueryClient();
  async function syncSavedRating(identity: RatingIdentity, saved: ModelRatingEntry | null) {
    await client.cancelQueries({ queryKey: MODEL_RATINGS_QUERY_KEY });
    // Backend keys are canonical; compare identities canonically so case or
    // numeric dot/hyphen variants replace their own cache row instead of duplicating it.
    const key = canonicalModelPrefix(identity.modelPrefix);
    client.setQueryData<ModelRatingEntry[]>(MODEL_RATINGS_QUERY_KEY, (previous) => {
      // Never invent a successful full list from a single-row mutation.
      if (!previous) return previous;
      const rest = previous.filter((entry) => entry.model_prefix !== key);
      return saved ? [...rest, saved].sort((a, b) => a.model_prefix.localeCompare(b.model_prefix)) : rest;
    });
    await invalidateModelRatings(client);
  }
  const save = useMutation({
    mutationFn: async ({ modelPrefix, score }: RatingIdentity & { score: number }) => {
      if (!Number.isInteger(score) || score < 0 || score > 100) throw new Error("Score must be an integer from 0 to 100.");
      const saved = await backend<unknown>("set_model_rating", { modelPrefix, input: { score } });
      // The backend lowercases and normalizes numeric dots before storing.
      if (!isModelRatingEntry(saved)
        || saved.model_prefix !== canonicalModelPrefix(modelPrefix)
        || saved.score !== score) {
        throw new Error("Invalid model rating response. Please retry loading ratings.");
      }
      return saved;
    },
    onSuccess: (saved, identity) => syncSavedRating(identity, saved),
  });
  const clear = useMutation({
    mutationFn: async ({ modelPrefix }: RatingIdentity) => {
      const response = await backend<{ ok?: boolean }>("delete_model_rating", { modelPrefix });
      if (response?.ok !== true) throw new Error("The backend did not confirm clearing the model rating.");
    },
    onSuccess: (_result, identity) => syncSavedRating(identity, null),
  });
  return { save, clear };
}
