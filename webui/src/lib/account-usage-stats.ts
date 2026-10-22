import { useQuery } from "@tanstack/react-query";
import { backend } from "./backend";
import type { AccountUsageStats } from "./types";

export type AccountUsagePeriod = "week" | "month";

/** Local calendar boundaries, not the upstream account's quota reset window. */
export function accountUsagePeriodStart(period: AccountUsagePeriod, now: number): number {
  const start = new Date(now);
  start.setHours(0, 0, 0, 0);
  if (period === "week") start.setDate(start.getDate() - (start.getDay() + 6) % 7);
  else start.setDate(1);
  return start.getTime();
}

/** Mount once per expanded management panel; every account shares this request. */
export function useAccountUsageStats(providerId: string, period: AccountUsagePeriod, now: number) {
  const periodStart = accountUsagePeriodStart(period, now);
  const query = useQuery<AccountUsageStats[]>({
    queryKey: ["provider-account-usage-stats", providerId, periodStart],
    queryFn: ({ signal }) => backend("get_provider_account_usage_stats", { id: providerId, periodStart }, { signal }),
    staleTime: 30_000,
    refetchInterval: 60_000,
    refetchOnWindowFocus: false,
  });
  return { query, periodStart, byAccount: new Map(query.data?.map((stats) => [stats.account_id, stats])) };
}
