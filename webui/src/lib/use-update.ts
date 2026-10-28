import { useQuery } from "@tanstack/react-query";
import { backend, IS_TAURI } from "@/lib/backend";
import type { UpdateCheck, VersionInfo } from "@/lib/update";

const SIX_HOURS = 6 * 60 * 60 * 1000;

export const VERSION_INFO_KEY = ["system-version"] as const;
export const UPDATE_CHECK_KEY = ["system-update-check"] as const;

/** Server version and self-update capability. Desktop builds update through Tauri, not this API. */
export function useVersionInfo() {
  return useQuery<VersionInfo>({
    queryKey: VERSION_INFO_KEY,
    queryFn: () => backend<VersionInfo>("get_system_version"),
    enabled: !IS_TAURI,
    staleTime: 60_000,
  });
}

/** Latest-release check, shared by the settings card and the sidebar badge. */
export function useUpdateCheck() {
  const { data: info } = useVersionInfo();
  return useQuery<UpdateCheck>({
    queryKey: UPDATE_CHECK_KEY,
    queryFn: () => backend<UpdateCheck>("check_system_update", { force: false }),
    enabled: !IS_TAURI && info?.update_check === true,
    staleTime: SIX_HOURS,
    refetchInterval: SIX_HOURS,
    refetchOnWindowFocus: false,
    retry: false,
  });
}
