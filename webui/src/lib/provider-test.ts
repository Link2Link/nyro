import { localizeBackendErrorMessage } from "./backend-error";
import type { AccountTestResult, TestResult } from "./types";

export type ProviderTestLog = {
  level: "info" | "success" | "error";
  /** Render as a React text child, never as HTML. */
  message: string;
};

export function providerTestError(error: string | null | undefined, isZh = false): string {
  switch (error) {
    case "Devin account is not authorized; sign in again":
      return isZh ? "Devin 账号尚未授权，请重新登录" : error;
    case "Devin account authentication was rejected; sign in again":
      return isZh ? "Devin 账号认证被拒绝，请重新登录" : error;
    case "Devin connectivity check failed; verify the account and proxy/network settings":
      return isZh ? "Devin 连通性检查失败，请检查账号与代理／网络设置" : error;
    case "Devin connectivity check timed out":
      return isZh ? "Devin 连通性检查超时" : error;
    case "No Devin accounts are connected":
      return isZh ? "没有已连接的 Devin 账号" : error;
    case "No enabled Devin accounts":
      return isZh ? "没有已启用的 Devin 账号" : error;
    case "No Devin account passed connectivity check":
      return isZh ? "没有 Devin 账号通过连通性检查" : error;
    default:
      return error?.trim()
        ? localizeBackendErrorMessage(error, isZh)
        : (isZh ? "连接失败" : "Connectivity check failed");
  }
}

/** Keep backend order and verdicts; skipped rows are not tested failures. */
export function formatAccountTestLogs(accounts?: AccountTestResult[], isZh = false): ProviderTestLog[] {
  if (!accounts) return []; // Legacy caches and non-pool providers have no account results.
  const logs: ProviderTestLog[] = [];
  let tested = 0;
  let passed = 0;
  let skipped = 0;
  for (const account of accounts) {
    const label = `${account.label.trim() || (isZh ? "账号" : "Account")} (${account.account_id.slice(0, 8)})`;
    if (account.skipped_reason) {
      skipped += 1;
      let reason: string;
      switch (account.skipped_reason) {
        case "disabled":
          reason = isZh ? "已禁用" : "Disabled";
          break;
        case "disconnected":
          reason = isZh ? "已断开连接" : "Disconnected";
          break;
        case "account_changed":
          reason = isZh ? "账号已变化，请重试" : "Account changed; retry the check";
          break;
        default:
          reason = isZh ? "未执行连通性检查" : "Connectivity check not performed";
      }
      logs.push({ level: "info", message: `${isZh ? "↷ 跳过账号" : "↷ Account skipped"} [${label}]: ${reason}` });
    } else {
      tested += 1;
      if (account.success) {
        passed += 1;
        logs.push({ level: "success", message: `${isZh ? "✓ 账号连通性正常" : "✓ Account reachable"} [${label}] ${account.latency_ms}ms` });
      } else {
        logs.push({ level: "error", message: `${isZh ? "✗ 账号连通性失败" : "✗ Account connectivity failed"} [${label}]: ${providerTestError(account.error, isZh)}` });
      }
    }
  }
  logs.push({
    level: "info",
    message: isZh
      ? `账号池：${passed}/${tested} 个已测试账号通过，${skipped} 个已跳过。仅检查连通性，不保证可用额度或模型权限。`
      : `Pool: ${passed}/${tested} tested passed, ${skipped} skipped. Connectivity only; does not guarantee quota or model access.`,
  });
  return logs;
}

export function formatProviderModelListLog(count: number, nativeCatalog: boolean, isZh = false): string {
  if (nativeCatalog) {
    return isZh
      ? `✓ 获取到 ${count} 个静态建议模型，不保证账号权限或可用额度`
      : `✓ Fetched ${count} static model suggestions; does not guarantee account permissions or available quota`;
  }
  return `${isZh ? "✓ 认证通过，获取到" : "✓ Auth valid, fetched"} ${count} ${isZh ? "个模型" : "models"}`;
}

/** Accept old cached results while retaining optional account diagnostics. */
export function normalizeCachedProviderTestResult(value: unknown): TestResult | null {
  if (!value || typeof value !== "object" || !("success" in value) || typeof value.success !== "boolean") return null;
  const result = value as TestResult;
  return {
    success: result.success,
    latency_ms: Number.isFinite(result.latency_ms) ? result.latency_ms : 0,
    model: typeof result.model === "string" ? result.model : undefined,
    error: typeof result.error === "string" ? result.error : undefined,
    endpoints: Array.isArray(result.endpoints) ? result.endpoints : undefined,
    accounts: Array.isArray(result.accounts) ? result.accounts : undefined,
  };
}
