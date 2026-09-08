import { useMemo } from "react";
import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { backend } from "./backend";
import { isProviderModelRating, providerModelKey, readProviderModelRatings, type RatingLoadState } from "./model-ratings";
import type { ProviderModelRating } from "./types";

export const MODEL_RATINGS_QUERY_KEY = ["provider-model-ratings"] as const;

export function invalidateModelRatings(client: QueryClient) {
  return client.invalidateQueries({ queryKey: MODEL_RATINGS_QUERY_KEY });
}

/** The same unfiltered list cache is shared by every rating surface; no per-row requests. */
export function useModelRatings() {
  const query = useQuery({
    queryKey: MODEL_RATINGS_QUERY_KEY,
    queryFn: async () => readProviderModelRatings(await backend<unknown>("list_provider_model_ratings")),
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

export function useModelRatingMutations() {
  const client = useQueryClient();
  async function syncSavedRating(identity: RatingIdentity, saved: ProviderModelRating | null) {
    await client.cancelQueries({ queryKey: MODEL_RATINGS_QUERY_KEY });
    const key = providerModelKey(identity.providerId, identity.model);
    client.setQueryData<ProviderModelRating[]>(MODEL_RATINGS_QUERY_KEY, (previous) => {
      // Never invent a successful full list from a single-row mutation.
      if (!previous) return previous;
      const next = previous.filter((rating) => providerModelKey(rating.provider_id, rating.upstream_model) !== key);
      return saved ? [...next, saved] : next;
    });
    await invalidateModelRatings(client);
  }
  const save = useMutation({
    mutationFn: async ({ providerId, model, score }: RatingIdentity & { score: number }) => {
      if (!Number.isInteger(score) || score < 0 || score > 100) throw new Error("Score must be an integer from 0 to 100.");
      const saved = await backend<unknown>("set_provider_model_rating", { providerId, model, input: { score } });
      if (!isProviderModelRating(saved) || saved.provider_id !== providerId || saved.upstream_model !== model || saved.score !== score) {
        throw new Error("Invalid model rating response. Please retry loading ratings.");
      }
      return saved;
    },
    onSuccess: (saved, identity) => syncSavedRating(identity, saved),
  });
  const clear = useMutation({
    mutationFn: async ({ providerId, model }: RatingIdentity) => {
      const response = await backend<{ ok?: boolean }>("delete_provider_model_rating", { providerId, model });
      if (response?.ok !== true) throw new Error("The backend did not confirm clearing the model rating.");
    },
    onSuccess: (_result, identity) => syncSavedRating(identity, null),
  });
  return { save, clear };
}
