import { useQuery } from "@tanstack/react-query";
import { backend } from "./backend";
import type { AccountUsageStats } from "./types";

/** Mount once per expanded management panel; the backend resolves each account's reset cycle. */
export function useAccountUsageStats(providerId: string) {
  const query = useQuery<AccountUsageStats[]>({
    queryKey: ["provider-account-usage-stats", providerId],
    queryFn: ({ signal }) => backend("get_provider_account_usage_stats", { id: providerId }, { signal }),
    staleTime: 30_000,
    refetchInterval: 60_000,
    refetchOnWindowFocus: false,
  });
  return { query, byAccount: new Map(query.data?.map((stats) => [stats.account_id, stats])) };
}
