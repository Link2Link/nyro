import { deepEqual, equal, throws } from "node:assert/strict";
import { test } from "node:test";
import type { AccountPoolView, ProviderAccountView } from "./devin-account-pool";
import {
  canConfirmModelProbe, canProbeAccount, initialProbeAccountId, probePoolAccounts,
  selectedProbeAccount, supportsProbeAccounts, type ProbeAccountsState, type ProbeProvider,
} from "./model-probe-accounts";

const provider: ProbeProvider = { id: "p", name: "Devin", vendor: "devin", auth_mode: "oauth" };
const account = (account_id: string, overrides: Partial<ProviderAccountView> = {}): ProviderAccountView => ({
  account_id, provider_id: "p", label: "Work", is_enabled: true, status: "connected", priority: 0,
  status_version: 1, verified_plan: null, identity_verified: false, scheduling: "unknown",
  daily: null, weekly: null, reserve_usd: null, observed_at: null, stale: true, updated_at: "", ...overrides,
});

// Pure helpers: no DOM or OAuth credentials are needed.
test("only the three supported OAuth identities require an account", () => {
  for (const identity of [
    { vendor: "devin" }, { vendor: "kiro" }, { preset_key: " KIro " },
    { vendor: "anthropic", channel: "claude-code" },
  ]) {
    const p = { ...provider, vendor: undefined, ...identity };
    equal(supportsProbeAccounts(p), true);
    equal(supportsProbeAccounts({ ...p, auth_mode: "apikey" }), false);
    equal(supportsProbeAccounts({ ...p, auth_mode: undefined }), false);
  }
  equal(supportsProbeAccounts({ ...provider, vendor: "openai" }), false);
  equal(supportsProbeAccounts({ ...provider, vendor: "anthropic", channel: "api" }), false);
});

test("only a single real enabled connected account can be preselected", () => {
  equal(initialProbeAccountId([]), "");
  equal(initialProbeAccountId([account("a")]), "a");
  equal(initialProbeAccountId([account("a"), account("b")]), "");
  equal(initialProbeAccountId([account("a"), account("b", { is_enabled: false })]), "");
  equal(initialProbeAccountId([account("a", { is_enabled: false })]), "");
  for (const status of ["disconnected", "pending", "not_connected", "refreshing", "error"]) {
    equal(canProbeAccount(account("a", { status })), false);
    equal(initialProbeAccountId([account("a", { status })]), "");
  }
});

test("loading, errors, empty pools and invalid selections cannot fall back to auto scheduling", () => {
  const blocked: ProbeAccountsState[] = [
    { status: "loading" }, { status: "error", error: "offline" }, { status: "ready", accounts: [] },
    { status: "ready", accounts: [account("a", { is_enabled: false })] },
    { status: "ready", accounts: [account("a", { status: "disconnected" })] },
    { status: "ready", accounts: [account("other")] },
  ];
  for (const state of blocked) {
    equal(selectedProbeAccount(state, "a"), undefined);
    equal(canConfirmModelProbe(provider, 1, state, "a"), false);
    equal(canConfirmModelProbe({ ...provider, auth_mode: "apikey" }, 1, state, "a"), true);
  }
  const ready: ProbeAccountsState = { status: "ready", accounts: [account("a"), account("b")] };
  equal(canConfirmModelProbe(provider, 1, ready, ""), false);
  equal(canConfirmModelProbe(provider, 1, ready, "b"), true);
  equal(canConfirmModelProbe(provider, 0, ready, "b"), false);
});

test("pool responses must belong to the current provider and contain real account ids", () => {
  const pool = { provider_id: "p", accounts: [account("a")] } as AccountPoolView;
  deepEqual(probePoolAccounts(pool, "p"), [account("a")]);
  throws(() => probePoolAccounts(pool, "different"), /Invalid account pool/);
  throws(() => probePoolAccounts(null, "p"), /Invalid account pool/);
  throws(() => probePoolAccounts({ ...pool, accounts: [account("", {})] }, "p"), /Invalid account pool/);
  throws(() => probePoolAccounts({ ...pool, accounts: [account("a", { provider_id: "old" })] }, "p"), /Invalid account pool/);
});
