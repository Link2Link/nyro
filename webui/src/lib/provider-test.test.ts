import { deepEqual, doesNotMatch, equal, match } from "node:assert/strict";
import { test } from "node:test";
import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  formatAccountTestLogs,
  formatProviderModelListLog,
  formatProviderTestSkip,
  isProviderTestSkipped,
  mergeProviderTestVerdict,
  normalizeCachedProviderTestResult,
  providerTestError,
  providerTestStatus,
} from "./provider-test";
import type { AccountTestResult, TestResult } from "./types";

function account(overrides: Partial<AccountTestResult> = {}): AccountTestResult {
  return {
    account_id: "12345678-1234-4567-8901-123456789012",
    label: "Team",
    success: true,
    latency_ms: 42,
    error: null,
    skipped_reason: null,
    tested_at: "2030-01-01T00:00:00Z",
    ...overrides,
  };
}

const retryAt = Date.parse("2030-01-02T03:04:05Z");
const cooldown = account({ success: false, skipped_reason: "usage_cooldown", retry_at: retryAt });
const skipped: TestResult = {
  success: false, latency_ms: 0, error: null, skipped_reason: "usage_cooldown", accounts: [cooldown],
};

for (const isZh of [false, true]) {
  const locale = isZh ? "Chinese" : "English";

  test(`${locale}: mixed results preserve order and distinguish duplicate aliases`, () => {
    const accounts = [
      account(),
      account({ account_id: "87654321-1234-4567-8901-123456789012", success: false, error: "Devin account is not authorized; sign in again" }),
      account({ account_id: "disabled-1234", success: false, skipped_reason: "disabled" }),
    ];
    const logs = formatAccountTestLogs(accounts, isZh);
    deepEqual(logs.map(({ level }) => level), ["success", "error", "info", "info"]);
    match(logs[0].message, /Team \(12345678\).*42ms/);
    match(logs[1].message, /Team \(87654321\)/);
    match(logs[2].message, /Team \(disabled\)/);
    match(logs[0].message, isZh ? /账号连通性正常/ : /Account reachable/);
    match(logs[1].message, isZh ? /账号连通性失败/ : /Account connectivity failed/);
    match(logs[2].message, isZh ? /跳过账号.*已禁用/ : /Account skipped.*Disabled/);
    match(logs[3].message, isZh ? /1\/2 个已测试账号通过，1 个已跳过/ : /1\/2 tested passed, 1 skipped/);
    match(logs[3].message, isZh ? /仅检查连通性，不保证可用额度或模型权限/ : /Connectivity only; does not guarantee quota or model access/);
  });

  test(`${locale}: cooldown skips show neutral bilingual retry times`, () => {
    const logs = formatAccountTestLogs([cooldown], isZh);
    deepEqual(logs.map(({ level }) => level), ["info", "info"]);
    match(logs[0].message, isZh ? /跳过账号.*用量查询冷却中.*可重试时间/ : /Account skipped.*Usage cooldown.*retry after/);
    const time = new Date(retryAt).toLocaleString(isZh ? "zh-CN" : "en-US", { timeZoneName: "short" });
    equal(logs[0].message.includes(time), true);
    const summary = formatProviderTestSkip({ ...skipped, accounts: [
      account({ ...cooldown, retry_at: retryAt + 60_000 }), cooldown,
    ] }, isZh);
    match(summary, isZh ? /连通性检查已跳过.*用量查询冷却中.*可重试时间/ : /Connectivity check skipped.*Usage cooldown.*retry after/);
    equal(summary.includes(time), true);
    doesNotMatch(summary, /✓|✗|failed|passed|成功|失败/);
    for (const retry_at of [undefined, null, NaN, Infinity, Number.MAX_VALUE]) {
      const missingTime = { ...skipped, accounts: [account({ ...cooldown, retry_at })] };
      match(formatProviderTestSkip(missingTime, isZh), isZh ? /请稍后重试/ : /retry later/);
      doesNotMatch(formatProviderTestSkip(missingTime, isZh), /Invalid Date|1970/);
    }
  });

  test(`${locale}: cooldowns do not hide actual failures or successful probes`, () => {
    const failure = account({ success: false, error: "Real authentication failure" });
    const failedLogs = formatAccountTestLogs([failure, cooldown], isZh);
    deepEqual(failedLogs.map(({ level }) => level), ["error", "info", "info"]);
    match(failedLogs[0].message, /Real authentication failure/);
    match(failedLogs[2].message, isZh ? /0\/1 个已测试账号通过，1 个已跳过/ : /0\/1 tested passed, 1 skipped/);
    const passedLogs = formatAccountTestLogs([account(), cooldown], isZh);
    deepEqual(passedLogs.map(({ level }) => level), ["success", "info", "info"]);
    match(passedLogs[2].message, isZh ? /1\/1 个已测试账号通过，1 个已跳过/ : /1\/1 tested passed, 1 skipped/);
  });

  test(`${locale}: all failed reports every safe error, with no successful probe`, () => {
    const logs = formatAccountTestLogs([
      account({ success: false, error: "Devin account is not authorized; sign in again" }),
      account({ account_id: "87654321-other", success: false, error: "Devin account authentication was rejected; sign in again" }),
      account({ account_id: "11223344-other", success: false, error: null }),
    ], isZh);
    deepEqual(logs.map(({ level }) => level), ["error", "error", "error", "info"]);
    match(logs[0].message, isZh ? /Devin 账号尚未授权，请重新登录/ : /Devin account is not authorized; sign in again/);
    match(logs[1].message, isZh ? /Devin 账号认证被拒绝，请重新登录/ : /Devin account authentication was rejected; sign in again/);
    match(logs[2].message, isZh ? /连接失败/ : /Connectivity check failed/);
    match(logs[3].message, isZh ? /0\/3 个已测试账号通过，0 个已跳过/ : /0\/3 tested passed, 0 skipped/);
    equal(providerTestError("No Devin account passed connectivity check", isZh),
      isZh ? "没有 Devin 账号通过连通性检查" : "No Devin account passed connectivity check");
  });

  test(`${locale}: disabled, disconnected and changed accounts are skips, not failures`, () => {
    const reasons = ["disabled", "disconnected", "account_changed"];
    const logs = formatAccountTestLogs(reasons.map((reason) => account({
      success: false, skipped_reason: reason, error: "Must not be displayed for a skipped row",
    })), isZh);
    deepEqual(logs.map(({ level }) => level), ["info", "info", "info", "info"]);
    match(logs[0].message, isZh ? /已禁用/ : /Disabled/);
    match(logs[1].message, isZh ? /已断开连接/ : /Disconnected/);
    match(logs[2].message, isZh ? /账号已变化，请重试/ : /Account changed; retry the check/);
    match(logs[3].message, isZh ? /0\/0 个已测试账号通过，3 个已跳过/ : /0\/0 tested passed, 3 skipped/);
    doesNotMatch(logs.map(({ message }) => message).join("\n"), /Must not be displayed|connectivity failed|连通性失败/);
    equal(providerTestError("No enabled Devin accounts", isZh),
      isZh ? "没有已启用的 Devin 账号" : "No enabled Devin accounts");
  });

  test(`${locale}: empty and legacy account results do not invent failures or successes`, () => {
    const empty = formatAccountTestLogs([], isZh);
    equal(empty.length, 1);
    equal(empty[0].level, "info");
    match(empty[0].message, isZh ? /0\/0 个已测试账号通过，0 个已跳过/ : /0\/0 tested passed, 0 skipped/);
    const legacy = normalizeCachedProviderTestResult({ success: true, latency_ms: 12 });
    equal(legacy?.accounts, undefined);
    deepEqual(formatAccountTestLogs(legacy?.accounts, isZh), []);
    deepEqual(formatAccountTestLogs(undefined, isZh), []);
  });

  test(`${locale}: fixed backend safety hints are localized without raw upstream details`, () => {
    const messages = [
      ["Devin account is not authorized; sign in again", "Devin 账号尚未授权，请重新登录"],
      ["Devin account authentication was rejected; sign in again", "Devin 账号认证被拒绝，请重新登录"],
      ["Devin connectivity check failed; verify the account and proxy/network settings", "Devin 连通性检查失败，请检查账号与代理／网络设置"],
      ["Devin connectivity check timed out", "Devin 连通性检查超时"],
      ["No Devin accounts are connected", "没有已连接的 Devin 账号"],
    ];
    for (const [en, zh] of messages) equal(providerTestError(en, isZh), isZh ? zh : en);
  });

  test(`${locale}: native catalogs never imply authorization, permissions or quota`, () => {
    const native = formatProviderModelListLog(7, true, isZh);
    match(native, isZh ? /7 个静态建议模型，不保证账号权限或可用额度/ : /7 static model suggestions; does not guarantee account permissions or available quota/);
    doesNotMatch(native, /认证通过|Auth valid/);
    // Non-native API-key / adaptive model discovery keeps its existing presentation.
    equal(formatProviderModelListLog(7, false, isZh), isZh ? "✓ 认证通过，获取到 7 个模型" : "✓ Auth valid, fetched 7 models");
  });

  test(`${locale}: legacy AMBIGUOUS_ACCOUNT is a useful localized hint, not a raw code`, () => {
    const expected = isZh ? "请在管理账号面板中选择具体账号。" : "Select a specific account in Manage accounts.";
    equal(providerTestError("AMBIGUOUS_ACCOUNT: more than one account", isZh), expected);
    equal(providerTestError('{"code":"AMBIGUOUS_ACCOUNT","message":"internal detail"}', isZh), expected);
    doesNotMatch(formatAccountTestLogs([account({ success: false, error: "AMBIGUOUS_ACCOUNT" })], isZh)[0].message, /AMBIGUOUS_ACCOUNT/);
  });

  test(`${locale}: account aliases and safe errors remain React text, never HTML`, () => {
    const label = '<img src=x onerror="alert(1)"> & Team';
    const error = "<script>alert('x')</script>";
    const logs = formatAccountTestLogs([account({ label, success: false, error })], isZh);
    equal(logs[0].message.includes(label), true);
    equal(logs[0].message.includes(error), true);
    // Matches ProvidersPage's existing <p>{log.message}</p> rendering.
    const html = renderToStaticMarkup(createElement("p", null, logs[0].message));
    match(html, /&lt;img src=x onerror=&quot;alert\(1\)&quot;&gt; &amp; Team/);
    match(html, /&lt;script&gt;/);
    doesNotMatch(html, /<img|<script/);
  });
}

test("English is the canonical default for logs, errors and catalogs", () => {
  deepEqual(formatAccountTestLogs([account()]), formatAccountTestLogs([account()], false));
  equal(providerTestError(null), "Connectivity check failed");
  equal(formatProviderModelListLog(3, true), formatProviderModelListLog(3, true, false));
  match(formatAccountTestLogs([account({ label: "  " })])[0].message, /Account \(12345678\)/);
});

test("cached DTO round trips account diagnostics, endpoint results and stable backend order", () => {
  const result: TestResult = {
    success: true, latency_ms: 80, model: "static-model",
    accounts: [account({ account_id: "second-first" }), account({ account_id: "first-second", success: false, skipped_reason: "account_changed" })],
    endpoints: [{ endpoint_id: "ep", protocol: "openai-compatible", base_url: "https://example.com", success: true, latency_ms: 80, tested_at: "2030-01-01T00:00:00Z" }],
  };
  const cached = normalizeCachedProviderTestResult(JSON.parse(JSON.stringify(result)));
  deepEqual(cached?.accounts, result.accounts);
  deepEqual(cached?.endpoints, result.endpoints);
  equal(cached?.model, result.model);
  deepEqual(formatAccountTestLogs(cached?.accounts), formatAccountTestLogs(result.accounts));
});

test("legacy malformed cache fields are ignored without requiring accounts", () => {
  for (const invalid of [null, undefined, "test", 12, {}, { success: "true" }]) {
    equal(normalizeCachedProviderTestResult(invalid), null);
  }
  const result = normalizeCachedProviderTestResult({ success: false, latency_ms: null, model: 1, error: {}, endpoints: {}, accounts: {} });
  deepEqual(result, { success: false, latency_ms: 0, model: undefined, error: undefined, skipped_reason: undefined, endpoints: undefined, accounts: undefined });
  deepEqual(normalizeCachedProviderTestResult({ success: false, latency_ms: 0, accounts: [] })?.accounts, []);
});

test("all-skipped is neutral; normal, mixed, transport and catalog errors retain their verdicts", () => {
  equal(isProviderTestSkipped(skipped), true);
  equal(providerTestStatus(skipped), "skipped");
  equal(providerTestStatus({ ...skipped, accounts: undefined }), "skipped");
  equal(providerTestStatus({ success: true, latency_ms: 42 }), "success");
  for (const error of [null, undefined, "Transport failure"]) {
    equal(providerTestStatus({ success: false, latency_ms: 0, error }), "failed");
  }
  const failure = account({ success: false, error: "Authentication rejected" });
  // A stray marker must not hide a real account failure, even without an aggregate error.
  equal(providerTestStatus({ ...skipped, accounts: [failure, cooldown] }), "failed");
  equal(providerTestStatus({ ...skipped, error: "Real error" }), "failed");
  equal(providerTestStatus({ success: true, latency_ms: 42, accounts: [account(), cooldown] }), "success");
  equal(providerTestStatus({ success: false, latency_ms: 42, error: "Catalog error", accounts: [account(), cooldown] }), "failed");
});

test("skipped and incomplete cooldown runs preserve previous cache without inventing a verdict", () => {
  const mixed: TestResult = {
    success: false, latency_ms: 42, error: "Authentication rejected",
    accounts: [account({ success: false, error: "Authentication rejected" }), cooldown],
  };
  const previousStates: Record<string, TestResult>[] = [
    {}, { p: { success: true, latency_ms: 42 } }, { p: { success: false, latency_ms: 0, error: "Prior error" } },
  ];
  for (const previous of previousStates) {
    const before = JSON.stringify(previous);
    equal(mergeProviderTestVerdict(previous, "p", skipped), previous);
    equal(mergeProviderTestVerdict(previous, "p", mixed), previous);
    equal(JSON.stringify(previous), before);
    equal(providerTestStatus(mixed), "failed"); // current failure remains visible, even when not persisted
  }
  const normal: TestResult[] = [
    { success: true, latency_ms: 42 },
    { success: false, latency_ms: 0, error: "Transport error" },
    { success: true, latency_ms: 42, accounts: [account(), cooldown] },
    { success: false, latency_ms: 42, error: "Catalog error", accounts: [account(), cooldown] },
  ];
  for (const result of normal) equal(mergeProviderTestVerdict({}, "p", result).p, result);
});

test("cache normalization retains explicit skips and retry metadata, never guesses old failures", () => {
  const restored = normalizeCachedProviderTestResult(JSON.parse(JSON.stringify(skipped)))!;
  equal(restored.success, false);
  equal(restored.error, null);
  equal(restored.skipped_reason, "usage_cooldown");
  equal(restored.accounts?.[0].retry_at, retryAt);
  equal(providerTestStatus(restored), "skipped");
  equal(formatProviderTestSkip(restored), formatProviderTestSkip(restored, false));
  for (const result of [
    { success: false, latency_ms: 0, error: "Usage request failed" },
    { success: false, latency_ms: 0 },
    { success: true, latency_ms: 42 },
  ]) {
    const cached = normalizeCachedProviderTestResult(result)!;
    equal(isProviderTestSkipped(cached), false);
    equal(cached.success, result.success);
  }
});

test("ProvidersPage wires neutral finish before failure/discovery and only persists actual verdicts", () => {
  const relative = "src/pages/providers.tsx";
  const file = [resolve(relative), resolve("webui", relative)].find(existsSync);
  if (!file) throw new Error("Run from webui or the repository root");
  const source = readFileSync(file, "utf8");
  const start = source.indexOf("async function handleTest(");
  const handler = source.slice(start, source.indexOf("async function runModelProbe(", start));
  const skip = handler.indexOf("if (isProviderTestSkipped(connectivity))");
  const failure = handler.indexOf("if (!connectivity.success)");
  const discovery = handler.indexOf('"test_provider_models"');
  equal(skip > 0 && skip < failure && failure < discovery, true);
  const skipBranch = handler.slice(skip, failure);
  match(skipBranch, /finish\(connectivity, formatProviderTestSkip\(connectivity, isZh\), "info"\)/);
  match(skipBranch, /return;/);
  doesNotMatch(skipBranch, /success: true|"error"/);
  match(handler, /setCurrentTestResult\(\(prev\) => \(\{ \.\.\.prev, \[provider.id\]: completed \}\)\)/);
  match(handler, /setTestResult\(\(prev\) => mergeProviderTestVerdict\(prev, provider.id, completed\)\)/);
  doesNotMatch(handler, /setTestResult\(\(prev\) => \{/); // no deleting the previous cached verdict at start
  match(source, /saveProviderTestResults\(testResult\)/);
  doesNotMatch(source, /saveProviderTestResults\(currentTestResult\)/);
  match(source, /if \(result && !isProviderTestSkipped\(result\)\) normalized\[id\] = result/);
  match(source, /const tr = currentTestResult\[p.id\] \?\? testResult\[p.id\]/);
  match(source, /\? providerTestStatus\(tr\)/);
  match(source, /status === "skipped" && tr \? \([\s\S]*?text-slate-500[\s\S]*?formatProviderTestSkip\(tr, isZh\)/);
  match(source, /providerNeedsAttention\(p, currentTestResult\[p.id\], testResult\[p.id\]\)/);
});
