type ErrorPayload = {
  code?: string;
  message?: string;
  params?: Record<string, unknown>;
};

function extractRawErrorMessage(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error && typeof error.message === "string") return error.message;
  if (error && typeof error === "object") {
    const obj = error as Record<string, unknown>;
    if (typeof obj.message === "string") return obj.message;
    if (typeof obj.error === "string") return obj.error;
  }
  return String(error);
}

function parseErrorPayload(raw: string): ErrorPayload | null {
  const text = raw.trim();
  if (!text.startsWith("{") || !text.endsWith("}")) return null;
  try {
    const parsed = JSON.parse(text) as ErrorPayload;
    if (!parsed || typeof parsed !== "object") return null;
    return parsed;
  } catch {
    return null;
  }
}

function extractName(params?: Record<string, unknown>) {
  if (!params || typeof params !== "object") return "";
  const value = params.name;
  return typeof value === "string" ? value : "";
}

function extractRouteCount(params?: Record<string, unknown>) {
  if (!params || typeof params !== "object") return 0;
  const value = params.routeCount;
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function extractProtocol(params?: Record<string, unknown>) {
  if (!params || typeof params !== "object") return "";
  const value = params.protocol;
  return typeof value === "string" ? value : "";
}

function extractModel(params?: Record<string, unknown>) {
  if (!params || typeof params !== "object") return "";
  const value = params.model;
  return typeof value === "string" ? value : "";
}

export function localizeBackendErrorMessage(error: unknown, isZh: boolean): string {
  const raw = extractRawErrorMessage(error);
  const payload = parseErrorPayload(raw);
  const code = payload?.code ?? raw.split(":", 1)[0];
  const accountMessages: Record<string, [string, string]> = {
    ACCOUNT_VERSION_CONFLICT: ["This account changed. Reload it and retry the operation.", "账号已变化，请重新读取后重试。"],
    AMBIGUOUS_ACCOUNT: ["Select a specific account in Manage accounts.", "请在管理账号面板中选择具体账号。"],
    POOL_PLAN_LOCKED: ["Disconnect all accounts before changing the pool plan.", "请先断开所有账号，再修改账号池套餐。"],
    POOL_NOT_CONFIGURED: ["Select a pool plan before adding or re-authorizing accounts.", "添加或重登账号前请先选择账号池套餐。"],
    PLAN_MISMATCH: ["This account's plan or quota windows do not match the pool.", "此账号套餐或额度窗口与账号池不匹配。"],
    PLAN_UNVERIFIED: ["The account plan could not be verified.", "无法验证此账号的套餐。"],
    ACCOUNT_SESSION_TARGET_MISMATCH: ["The login session does not belong to this account target.", "登录会话与目标账号不匹配。"],
  };
  if (accountMessages[code]) return accountMessages[code][isZh ? 1 : 0];
  if (!payload?.code) return raw;

  const name = extractName(payload.params);
  switch (payload.code) {
    case "PROVIDER_NAME_CONFLICT":
      return isZh
        ? `提供商名称已存在：${name || "（未提供）"}`
        : `Provider name already exists: ${name || "(unknown)"}`;
    case "ROUTE_NAME_CONFLICT":
      return isZh
        ? `路由名称已存在：${name || "（未提供）"}`
        : `Route name already exists: ${name || "(unknown)"}`;
    case "API_KEY_NAME_CONFLICT":
      return isZh
        ? `API Key 名称已存在：${name || "（未提供）"}`
        : `API key name already exists: ${name || "(unknown)"}`;
    case "PROVIDER_IN_USE": {
      const routeCount = extractRouteCount(payload.params);
      if (isZh) {
        return routeCount > 0
          ? `该提供商正在被 ${routeCount} 条路由使用，无法删除。请先修改或删除相关路由。`
          : "该提供商正在被路由或其他资源引用，无法删除。请先解除引用后重试。";
      }
      return routeCount > 0
        ? `This provider is used by ${routeCount} routes and cannot be deleted. Update or delete those routes first.`
        : "This provider is still referenced by routes or other resources and cannot be deleted yet.";
    }
    case "ROUTE_PROTOCOL_MODEL_CONFLICT": {
      const protocol = extractProtocol(payload.params) || "openai";
      const model = extractModel(payload.params) || "(unknown)";
      return isZh
        ? `路由已存在：接入协议 ${protocol} + 虚拟模型 ID ${model} 的组合已被使用。`
        : `Route already exists: ingress protocol ${protocol} + virtual model ID ${model} is already used.`;
    }
    default:
      return payload.message || raw;
  }
}
