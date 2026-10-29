import { deepEqual, equal, ok } from "node:assert/strict";
import { test } from "node:test";
import {
  newDevinPoolConfig,
  type AccountPoolView,
  type DevinPlan,
  type PoolQuotaWindow,
  type ProviderAccountView,
} from "./devin-account-pool";
import {
  claudePoolSteadyPacePercent,
  poolSteadyPacePercent,
  steadyPacePercent,
  tierWindowMs,
  utilizationBarClass,
  utilizationColor,
} from "./usage-pace";

const NOW = Date.parse("2030-06-15T12:00:00Z");
const HOUR = 3600_000;
const DAY = 24 * HOUR;
const WEEK = 7 * DAY;
const resetAt = (windowMs: number, elapsedPercent: number) =>
  new Date(NOW + windowMs * (1 - elapsedPercent / 100)).toISOString();

function account(
  id: string,
  plan: DevinPlan | null = "pro",
  dailyPace = 50,
  weeklyPace = 50,
  overrides: Partial<ProviderAccountView> = {},
): ProviderAccountView {
  return {
    account_id: id, provider_id: "devin", label: id, is_enabled: true, priority: 0,
    status: "connected", status_version: 1, verified_plan: plan, identity_verified: true,
    scheduling: "included", daily: { remaining_percent: 65, resets_at: resetAt(DAY, dailyPace) },
    weekly: { remaining_percent: 65, resets_at: resetAt(WEEK, weeklyPace) },
    reserve_usd: null, observed_at: NOW / 1000, stale: false,
    updated_at: new Date(NOW).toISOString(), ...overrides,
  };
}

function completeWindow(totalUnits = 1, totalAccounts = 1): PoolQuotaWindow {
  return {
    total_units: totalUnits, total_accounts: totalAccounts, capacity_complete: true,
    known_accounts: totalAccounts, known_remaining_units: totalUnits * 0.65,
    used_units: totalUnits * 0.35, remaining_units: totalUnits * 0.65,
    used_percent: 35, remaining_percent: 65, stale: false,
    next_reset_at: resetAt(WEEK, 50), next_reset_accounts: totalAccounts, reset_mode: "aligned",
  };
}

function pool(
  accounts = [account("pro")],
  windows: Partial<AccountPoolView["quota"]> = {},
): AccountPoolView {
  return {
    provider_id: "devin", config: newDevinPoolConfig(), accounts,
    quota: { daily: completeWindow(), weekly: completeWindow(), estimated: true, ...windows },
    counts: {
      total: accounts.length, accounting: accounts.length, available: accounts.length,
      included: accounts.length, reserve: 0, pro: 0, max: 0, unclassified: 0,
      unknown: 0, disabled: 0, disconnected: 0, auth_required: 0, quota_exhausted: 0,
      plan_mismatch: 0, plan_unverified: 0,
    },
    readiness: "included",
  };
}

function claudeAccount(
  id: string,
  plan: string | null = "pro",
  sessionPace = 50,
  weeklyPace = 50,
  overrides: Partial<ProviderAccountView> = {},
): ProviderAccountView {
  return account(id, null, 50, 50, {
    provider_id: "claude", daily: null, weekly: null,
    claude_usage: {
      plan,
      windows: [
        { name: "five_hour", used_percent: 35, resets_at: resetAt(5 * HOUR, sessionPace) },
        { name: "weekly_limit", used_percent: 35, resets_at: resetAt(WEEK, weeklyPace) },
      ],
    },
    ...overrides,
  });
}

function claudePool(accounts = [claudeAccount("pro")], sessionUnits = 1, weeklyUnits = 1, totalAccounts = accounts.length): AccountPoolView {
  return {
    ...pool(accounts), kind: "claude_code", provider_id: "claude", config: null,
    quota: {
      daily: null, weekly: null, estimated: true,
      claude_windows: [
        { name: "five_hour", quota: completeWindow(sessionUnits, totalAccounts) },
        { name: "weekly_limit", quota: completeWindow(weeklyUnits, totalAccounts) },
      ],
    },
  };
}

function close(actual: number | null, expected: number) {
  ok(actual != null, "Expected a visible pace reference");
  ok(Math.abs(actual - expected) < 1e-9, `${actual} should equal ${expected}`);
}

test("tier windows retain five-hour, daily, weekly and thirty-day monthly semantics", () => {
  for (const [name, duration] of [
    ["five_hour", 5 * HOUR], ["daily", DAY], ["weekly_limit", WEEK], ["monthly", 30 * DAY],
  ] as const) {
    equal(tierWindowMs(name), duration);
    for (const elapsed of [0, 25, 50, 75, 100]) {
      equal(steadyPacePercent({ name, resets_at: resetAt(duration, elapsed) }, NOW), elapsed);
    }
  }
  for (const name of ["", "weekly", "unknown", "feature:daily", "feature:weekly_limit", "feature:monthly"]) {
    equal(tierWindowMs(name), null);
    equal(steadyPacePercent({ name, resets_at: resetAt(DAY, 50) }, NOW), null);
  }
});

test("tier pace rejects missing/invalid resets and invalid clocks instead of returning NaN", () => {
  for (const resets_at of [undefined, null, "", "not-a-date", "2030-13-15T12:00:00Z"]) {
    equal(steadyPacePercent({ name: "daily", resets_at }, NOW), null);
  }
  for (const now of [NaN, Infinity, -Infinity, null as unknown as number, undefined as unknown as number]) {
    equal(steadyPacePercent({ name: "daily", resets_at: resetAt(DAY, 50) }, now), null);
  }
});

test("tier pace clamps future windows to zero and expired snapshots to one hundred", () => {
  const tier = { name: "daily", resets_at: resetAt(DAY, 50) };
  equal(steadyPacePercent(tier, NOW - DAY), 0);
  equal(steadyPacePercent(tier, NOW + DAY / 2), 100);
  equal(steadyPacePercent(tier, NOW + 100 * DAY), 100);
  equal(steadyPacePercent({ name: "daily", resets_at: resetAt(DAY, -200) }, NOW), 0);
});

test("utilization colors retain existing exact threshold classes", () => {
  for (const [percent, text, bar] of [
    [-1, "text-emerald-600", "bg-emerald-500"],
    [0, "text-emerald-600", "bg-emerald-500"],
    [69.99, "text-emerald-600", "bg-emerald-500"],
    [70, "text-orange-500", "bg-orange-500"],
    [89.99, "text-orange-500", "bg-orange-500"],
    [90, "text-red-500", "bg-red-500"],
    [100, "text-red-500", "bg-red-500"],
    [150, "text-red-500", "bg-red-500"],
  ] as const) {
    equal(utilizationColor(percent), text);
    equal(utilizationBarClass(percent), bar);
  }
});

test("mixed staggered resets weight Pro 25/75 and Max 50 by capacity to fifty percent", () => {
  const mixed = pool([
    account("pro-a", "pro", 25, 25),
    account("pro-b", "pro", 75, 75),
    account("max", "max", 0, 50),
  ], {
    daily: completeWindow(2, 2),
    weekly: { ...completeWindow(12, 3), reset_mode: "staggered", next_reset_at: resetAt(WEEK, 75), next_reset_accounts: 1 },
  });
  equal(poolSteadyPacePercent(mixed, "weekly", NOW), 50);
  equal(poolSteadyPacePercent(mixed, "daily", NOW), 50);
});

test("weekly Pro at 100 plus Max at zero uses 1:10 capacity, not an arithmetic mean", () => {
  const mixed = pool([account("pro", "pro", 50, 100), account("max", "max", 50, 0)], {
    weekly: completeWindow(11, 2),
  });
  close(poolSteadyPacePercent(mixed, "weekly", NOW), 100 / 11);
  // Actual usage stays the authoritative DTO value, unrelated to projected pace.
  equal(mixed.quota.weekly!.used_percent, 35);
});

test("daily uses only Pro resets and ignores a verified Max with no daily quota", () => {
  const mixed = pool([
    account("pro", "pro", 25),
    account("max", "max", 100, 100, { daily: null }),
  ], { weekly: completeWindow(11, 2) });
  equal(poolSteadyPacePercent(mixed, "daily", NOW), 25);
  mixed.accounts[1].daily = { remaining_percent: NaN, resets_at: "invalid" };
  equal(poolSteadyPacePercent(mixed, "daily", NOW), 25);
});

test("aligned account resets give the same pace regardless of capacity", () => {
  const aligned = pool([account("pro", "pro", 75, 75), account("max", "max", 75, 75)], {
    weekly: completeWindow(11, 2),
  });
  equal(poolSteadyPacePercent(aligned, "weekly", NOW), 75);
  equal(poolSteadyPacePercent(aligned, "daily", NOW), 75);
});

test("next_reset_at is never treated as a common pool reset, even when accounts are staggered", () => {
  const mixed = pool([account("pro", "pro", 50, 100), account("max", "max", 50, 0)], {
    weekly: { ...completeWindow(11, 2), reset_mode: "staggered", next_reset_accounts: 1 },
  });
  for (const next of [resetAt(WEEK, 100), resetAt(WEEK, 0), "invalid", null]) {
    mixed.quota.weekly!.next_reset_at = next;
    close(poolSteadyPacePercent(mixed, "weekly", NOW), 100 / 11);
  }
  mixed.accounts[0].weekly!.resets_at = null;
  mixed.quota.weekly!.next_reset_at = resetAt(WEEK, 50);
  equal(poolSteadyPacePercent(mixed, "weekly", NOW), null);
});

test("unknown plans hide both references instead of guessing Pro or skipping an unknown Max daily", () => {
  const unknown = pool([account("unknown", null)]);
  // Even a misleading complete DTO or legacy configured plan cannot supply identity.
  unknown.config!.plan = "max";
  for (const name of ["daily", "weekly"] as const) {
    equal(poolSteadyPacePercent(unknown, name, NOW), null);
  }
  unknown.accounts[0].daily = null;
  equal(poolSteadyPacePercent(unknown, "daily", NOW), null);
  const mixed = pool([account("pro"), account("unknown-max", null, 50, 50, { daily: null })]);
  equal(poolSteadyPacePercent(mixed, "daily", NOW), null);
});

test("missing or invalid member quota/reset suppresses references with otherwise complete DTOs", () => {
  for (const name of ["daily", "weekly"] as const) {
    const subject = pool();
    subject.accounts[0][name] = null;
    equal(poolSteadyPacePercent(subject, name, NOW), null);
    for (const reset of [null, "", "invalid", "2030-99-15T12:00:00Z"]) {
      subject.accounts[0][name] = { remaining_percent: 65, resets_at: reset };
      equal(poolSteadyPacePercent(subject, name, NOW), null);
    }
    for (const remaining of [NaN, Infinity, -Infinity, null as unknown as number]) {
      subject.accounts[0][name] = { remaining_percent: remaining, resets_at: resetAt(WEEK, 50) };
      equal(poolSteadyPacePercent(subject, name, NOW), null);
    }
  }
});

test("pool DTO must have a window, complete positive capacity, matching membership and usage coverage", () => {
  const invalidWindows: Partial<PoolQuotaWindow>[] = [
    { capacity_complete: false },
    ...[0, -1, NaN, Infinity, -Infinity].map((total_units) => ({ total_units })),
    ...[0, 2, NaN, 1.5].map((total_accounts) => ({ total_accounts })),
    ...[0, 2, NaN].map((known_accounts) => ({ known_accounts })),
    { total_units: 10 }, // Finite and positive, but not this Pro account's capacity.
  ];
  for (const name of ["daily", "weekly"] as const) {
    equal(poolSteadyPacePercent(pool(undefined, { [name]: null }), name, NOW), null);
    for (const invalid of invalidWindows) {
      equal(poolSteadyPacePercent(pool(undefined, { [name]: { ...completeWindow(), ...invalid } }), name, NOW), null);
    }
    for (const field of ["used_percent", "remaining_percent", "used_units", "remaining_units", "known_remaining_units"] as const) {
      for (const value of [null, undefined, NaN, Infinity, -Infinity]) {
        const window = { ...completeWindow(), [field]: value } as PoolQuotaWindow;
        equal(poolSteadyPacePercent(pool(undefined, { [name]: window }), name, NOW), null, `${name}.${field}=${value}`);
      }
    }
  }
});

test("empty pools and all-Max daily windows never produce a reference", () => {
  equal(poolSteadyPacePercent(pool([], { weekly: completeWindow(0, 0) }), "weekly", NOW), null);
  const maxOnly = pool([account("max", "max")], { daily: null, weekly: completeWindow(10, 1) });
  equal(poolSteadyPacePercent(maxOnly, "daily", NOW), null);
  equal(poolSteadyPacePercent(maxOnly, "weekly", NOW), 50);
  maxOnly.quota.daily = completeWindow(0, 0);
  equal(poolSteadyPacePercent(maxOnly, "daily", NOW), null);
  maxOnly.quota.daily = completeWindow(10, 0);
  equal(poolSteadyPacePercent(maxOnly, "daily", NOW), null);
});

test("exhausted and auth-invalid accounts remain in the capacity denominator", () => {
  const subject = pool([
    account("exhausted", "pro", 100, 100, {
      scheduling: "quota_exhausted", daily: { remaining_percent: 0, resets_at: resetAt(DAY, 100) },
      weekly: { remaining_percent: 0, resets_at: resetAt(WEEK, 100) },
    }),
    account("auth-invalid", "max", 0, 0, { status: "authinvalid", scheduling: "auth_required" }),
  ], { weekly: completeWindow(11, 2) });
  subject.counts.available = 0;
  subject.counts.included = 0;
  close(poolSteadyPacePercent(subject, "weekly", NOW), 100 / 11);
  equal(poolSteadyPacePercent(subject, "daily", NOW), 100);
  subject.accounts[1].status = "error";
  close(poolSteadyPacePercent(subject, "weekly", NOW), 100 / 11);
});

test("disabled and disconnected accounts do not affect coverage, unknown plans or resets", () => {
  const subject = pool([
    account("pro", "pro", 25, 75),
    account("disabled-unknown", null, 0, 0, { is_enabled: false, daily: null, weekly: null }),
    account("disconnected-unknown", null, 0, 0, { status: "disconnected", daily: null, weekly: null }),
    account("disabled-max", "max", 0, 0, { is_enabled: false }),
  ]);
  equal(poolSteadyPacePercent(subject, "daily", NOW), 25);
  equal(poolSteadyPacePercent(subject, "weekly", NOW), 75);
});

test("time advances the projection without mutating quotas or reviving expired exhausted accounts", () => {
  const subject = pool([account("pro", "pro", 25, 25, { scheduling: "quota_exhausted" })]);
  for (const name of ["daily", "weekly"] as const) {
    const duration = name === "daily" ? DAY : WEEK;
    subject.accounts[0][name]!.remaining_percent = 0;
    const window = subject.quota[name]!;
    Object.assign(window, { used_percent: 100, remaining_percent: 0, used_units: 1, remaining_units: 0, known_remaining_units: 0 });
    const before = structuredClone(subject);
    equal(poolSteadyPacePercent(subject, name, NOW), 25);
    equal(poolSteadyPacePercent(subject, name, NOW + duration / 4), 50);
    equal(poolSteadyPacePercent(subject, name, NOW + 3 * duration / 4), 100);
    equal(poolSteadyPacePercent(subject, name, NOW + 2 * duration), 100);
    deepEqual(subject, before);
  }
});

test("all resets further than one window in the future clamp to zero, and invalid now hides pace", () => {
  const future = pool([account("pro", "pro", -50, -50), account("max", "max", -100, -100)], {
    weekly: completeWindow(11, 2),
  });
  for (const name of ["daily", "weekly"] as const) {
    equal(poolSteadyPacePercent(future, name, NOW), 0);
    for (const now of [NaN, Infinity, -Infinity, null as unknown as number]) {
      equal(poolSteadyPacePercent(future, name, now), null);
    }
  }
});

test("stale but complete snapshots retain their visual time projection without inferring fresh quota", () => {
  const subject = pool([account("pro", "pro", 75, 75, { stale: true })], {
    daily: { ...completeWindow(), stale: true }, weekly: { ...completeWindow(), stale: true },
  });
  equal(poolSteadyPacePercent(subject, "daily", NOW), 75);
  equal(poolSteadyPacePercent(subject, "weekly", NOW), 75);
});

test("Claude mixed plans use distinct five-hour 1:5:20 and weekly 1:5:10 weights", () => {
  const subject = claudePool([
    claudeAccount("pro", "pro", 100, 100),
    claudeAccount("five", "max 5x", 50, 50),
    claudeAccount("twenty", "max 20x", 0, 0),
  ], 26, 16);
  close(claudePoolSteadyPacePercent(subject, "five_hour", NOW), 350 / 26);
  close(claudePoolSteadyPacePercent(subject, "weekly_limit", NOW), 350 / 16);
});

test("Claude unknown and unsupported plans use one unit without changing plan metadata", () => {
  for (const plan of [undefined, null, "", "unknown", "max", "team", "max5x", "MAX 20X"]) {
    const subject = claudePool([
      claudeAccount("five", "max 5x", 0, 0),
      claudeAccount("fallback", plan ?? null, 100, 100),
    ], 6, 6);
    subject.accounts[1].claude_usage!.plan = plan;
    subject.accounts[1].verified_plan = "max"; // Devin identity is not Claude capacity.
    const before = structuredClone(subject);
    for (const name of ["five_hour", "weekly_limit"]) {
      close(claudePoolSteadyPacePercent(subject, name, NOW), 100 / 6);
    }
    deepEqual(subject, before);
  }
});

test("Claude excludes disabled/disconnected accounts but includes exhausted members", () => {
  const subject = claudePool([
    claudeAccount("exhausted", "pro", 100, 100, { scheduling: "quota_exhausted" }),
    claudeAccount("five", "max 5x", 0, 0),
    claudeAccount("disabled", null, 0, 0, { is_enabled: false, stale: true, claude_usage: null }),
    claudeAccount("disconnected", null, 0, 0, { status: "disconnected", scheduling: "auth_required", claude_usage: null }),
  ], 6, 6, 2);
  for (const window of subject.accounts[0].claude_usage!.windows) window.used_percent = 100;
  subject.counts.available = 0;
  for (const name of ["five_hour", "weekly_limit"]) {
    close(claudePoolSteadyPacePercent(subject, name, NOW), 100 / 6);
  }
});

test("Claude uses distinct member resets, never aggregate next_reset_at", () => {
  const subject = claudePool([
    claudeAccount("early", "pro", 75, 75), claudeAccount("late", "pro", 25, 25),
  ], 2, 2);
  for (const aggregate of subject.quota.claude_windows!) {
    Object.assign(aggregate.quota, { reset_mode: "staggered", next_reset_accounts: 1 });
    for (const next of [resetAt(WEEK, 100), resetAt(WEEK, 0), "invalid", null]) {
      aggregate.quota.next_reset_at = next;
      equal(claudePoolSteadyPacePercent(subject, aggregate.name, NOW), 50);
    }
    subject.accounts[0].claude_usage!.windows.find((window) => window.name === aggregate.name)!.resets_at = null;
    aggregate.quota.next_reset_at = resetAt(WEEK, 50);
    equal(claudePoolSteadyPacePercent(subject, aggregate.name, NOW), null);
  }
});

test("Claude supports only its two main windows and requires unique matching aggregates", () => {
  const subject = claudePool();
  for (const name of ["", "daily", "weekly", "monthly", "feature:five_hour", "seven_day_sonnet"]) {
    subject.quota.claude_windows!.push({ name, quota: completeWindow() });
    equal(claudePoolSteadyPacePercent(subject, name, NOW), null);
  }
  for (const name of ["five_hour", "weekly_limit"]) {
    for (const claude_windows of [undefined, [], null as unknown as NonNullable<AccountPoolView["quota"]["claude_windows"]>]) {
      equal(claudePoolSteadyPacePercent({ ...subject, quota: { ...subject.quota, claude_windows } }, name, NOW), null);
    }
    const aggregate = subject.quota.claude_windows!.find((window) => window.name === name)!;
    subject.quota.claude_windows!.push(aggregate);
    equal(claudePoolSteadyPacePercent(subject, name, NOW), null);
    subject.quota.claude_windows!.pop();
    aggregate.quota = null as unknown as PoolQuotaWindow;
    equal(claudePoolSteadyPacePercent(subject, name, NOW), null);
  }
});

test("Claude rejects incomplete, stale, invalid or mismatched aggregate coverage and capacity", () => {
  const invalidWindows: Partial<PoolQuotaWindow>[] = [
    { capacity_complete: false }, { stale: true },
    ...[0, -1, NaN, Infinity, -Infinity, 5].map((total_units) => ({ total_units })),
    ...[0, 2, NaN, 1.5].map((total_accounts) => ({ total_accounts })),
    ...[0, 2, NaN].map((known_accounts) => ({ known_accounts })),
  ];
  for (const field of ["used_percent", "remaining_percent", "used_units", "remaining_units", "known_remaining_units"] as const) {
    for (const value of [null, undefined, NaN, Infinity, -Infinity, -1, 101]) {
      invalidWindows.push({ [field]: value });
    }
  }
  invalidWindows.push({ used_units: 2 }, { remaining_units: 2 }, { known_remaining_units: 2 });
  for (const name of ["five_hour", "weekly_limit"]) {
    for (const invalid of invalidWindows) {
      const subject = claudePool();
      Object.assign(subject.quota.claude_windows!.find((window) => window.name === name)!.quota, invalid);
      equal(claudePoolSteadyPacePercent(subject, name, NOW), null, `${name}: ${JSON.stringify(invalid)}`);
    }
    equal(claudePoolSteadyPacePercent(claudePool([], 0, 0), name, NOW), null);
  }
});

test("Claude rejects missing/duplicate windows, invalid usage/resets, stale and auth-required members", () => {
  for (const name of ["five_hour", "weekly_limit"]) {
    for (const overrides of [
      { stale: true }, { scheduling: "auth_required" }, { claude_usage: null },
      { claude_usage: undefined }, { claude_usage: { plan: "pro", windows: [] } },
    ]) {
      equal(claudePoolSteadyPacePercent(claudePool([claudeAccount("pro", "pro", 50, 50, overrides)]), name, NOW), null);
    }
    const subject = claudePool();
    const windows = subject.accounts[0].claude_usage!.windows;
    const window = windows.find((window) => window.name === name)!;
    windows.push({ ...window });
    equal(claudePoolSteadyPacePercent(subject, name, NOW), null);
    windows.pop();
    for (const used of [NaN, Infinity, -Infinity, -1, 101, null, undefined]) {
      window.used_percent = used as number;
      equal(claudePoolSteadyPacePercent(subject, name, NOW), null);
    }
    window.used_percent = 35;
    for (const reset of [null, undefined, "", "invalid", "2030-99-15T12:00:00Z"]) {
      window.resets_at = reset as string | null;
      equal(claudePoolSteadyPacePercent(subject, name, NOW), null);
    }
  }
});

test("Claude clock changes move only the marker, clamped at bounds without reviving exhausted usage", () => {
  const subject = claudePool([claudeAccount("spent", "pro", 25, 25, { scheduling: "quota_exhausted" })]);
  for (const window of subject.accounts[0].claude_usage!.windows) window.used_percent = 100;
  for (const aggregate of subject.quota.claude_windows!) {
    Object.assign(aggregate.quota, { used_percent: 100, remaining_percent: 0, used_units: 1, remaining_units: 0, known_remaining_units: 0 });
  }
  const before = structuredClone(subject);
  for (const [name, duration] of [["five_hour", 5 * HOUR], ["weekly_limit", WEEK]] as const) {
    equal(claudePoolSteadyPacePercent(subject, name, NOW), 25);
    equal(claudePoolSteadyPacePercent(subject, name, NOW + duration / 4), 50);
    equal(claudePoolSteadyPacePercent(subject, name, NOW - duration), 0);
    equal(claudePoolSteadyPacePercent(subject, name, NOW + duration * 3 / 4), 100);
    equal(claudePoolSteadyPacePercent(subject, name, NOW + 10 * duration), 100);
    for (const now of [NaN, Infinity, -Infinity, null, undefined]) {
      equal(claudePoolSteadyPacePercent(subject, name, now as number), null);
    }
  }
  deepEqual(subject, before);
});
