import { isClaudeCodeOAuthProvider } from "./claude-account-pool";
import type { AccountPoolView, ProviderAccountView } from "./devin-account-pool";
import { isDevinProvider, isKiroProvider } from "./native-provider";
import type { Provider } from "./types";

export type ProbeProvider = Pick<Provider, "id" | "name" | "vendor" | "preset_key" | "channel" | "auth_mode">;

/** API-key providers must never acquire an OAuth account requirement. */
export function supportsProbeAccounts(provider: ProbeProvider): boolean {
  return provider.auth_mode === "oauth"
    && (isDevinProvider(provider) || isKiroProvider(provider) || isClaudeCodeOAuthProvider(provider));
}

export type ProbeAccountsState =
  | { status: "loading" }
  | { status: "error"; error: string }
  | { status: "ready"; accounts: ProviderAccountView[] };

/** Reject a stale/mismatched pool instead of silently invoking auto scheduling. */
export function probePoolAccounts(pool: AccountPoolView | null, providerId: string): ProviderAccountView[] {
  if (!pool || pool.provider_id !== providerId || !Array.isArray(pool.accounts)
    || pool.accounts.some((account) => account.provider_id !== providerId || !account.account_id)) {
    throw new Error("Invalid account pool response");
  }
  return pool.accounts;
}

export function canProbeAccount(account: ProviderAccountView): boolean {
  return account.is_enabled && account.status === "connected";
}

/** Multiple real accounts always require a choice, even with one eligible member. */
export function initialProbeAccountId(accounts: ProviderAccountView[]): string {
  return accounts.length === 1 && canProbeAccount(accounts[0]) ? accounts[0].account_id : "";
}

export function selectedProbeAccount(state: ProbeAccountsState, accountId: string): ProviderAccountView | undefined {
  return state.status === "ready"
    ? state.accounts.find((account) => account.account_id === accountId && canProbeAccount(account))
    : undefined;
}

export function canConfirmModelProbe(
  provider: ProbeProvider,
  modelCount: number,
  state: ProbeAccountsState,
  accountId: string,
): boolean {
  return modelCount > 0 && (!supportsProbeAccounts(provider) || Boolean(selectedProbeAccount(state, accountId)));
}
