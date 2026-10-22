import { getAdminToken, clearAdminToken } from "./auth";

const IS_TAURI = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

interface BackendOptions {
  signal?: AbortSignal;
}

export class BackendHTTPError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
    this.name = "BackendHTTPError";
  }
}

async function invokeIPC<T>(cmd: string, args?: Record<string, unknown>, options?: BackendOptions): Promise<T> {
  options?.signal?.throwIfAborted();
  const { invoke } = await import("@tauri-apps/api/core");
  options?.signal?.throwIfAborted();
  const result = await invoke<T>(cmd, args);
  // IPC itself cannot be aborted; never return a stale result to the caller.
  options?.signal?.throwIfAborted();
  return result;
}

async function invokeHTTP<T>(cmd: string, args?: Record<string, unknown>, options?: BackendOptions): Promise<T> {
  const mapping = resolveHTTP(cmd, args);
  const init: RequestInit = { method: mapping.method, signal: options?.signal };

  const headers: Record<string, string> = {};
  const token = getAdminToken();
  if (token) {
    headers["Authorization"] = `Bearer ${token}`;
  }
  if (mapping.body) {
    headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(mapping.body);
  }
  if (Object.keys(headers).length > 0) {
    init.headers = headers;
  }

  const resp = await fetch(mapping.url, init);

  if (resp.status === 401 && window.location.pathname !== "/login") {
    // A 401 may mean the admin session expired, or the endpoint is
    // reporting an upstream credential failure (e.g. a provider whose API
    // key was rejected by its usage service). Re-validate the admin session
    // before wiping the token so only a real session loss forces a logout.
    const adminSessionValid = await probeAdminSession();
    options?.signal?.throwIfAborted();
    if (adminSessionValid) {
      const body = await resp.json().catch(() => ({}));
      throw new BackendHTTPError(body.error || "HTTP 401", 401);
    }
    clearAdminToken();
    window.location.replace("/login");
    throw new Error("Authentication required");
  }

  if (!resp.ok) {
    const body = await resp.json().catch(() => ({}));
    throw new BackendHTTPError(body.error || `HTTP ${resp.status}`, resp.status);
  }
  const text = await resp.text();
  if (!text) return {} as T;
  const json = JSON.parse(text);
  if (json && typeof json === "object" && "error" in json) {
    const errorMessage =
      typeof json.error === "string" && json.error.trim()
        ? json.error
        : `Request failed: ${cmd}`;
    throw new Error(errorMessage);
  }
  // Use explicit key check instead of ??, so that { "data": null } correctly
  // returns null rather than falling back to the full response object.
  return json && typeof json === "object" && "data" in json ? json.data : json;
}

interface HTTPMapping {
  method: string;
  url: string;
  body?: Record<string, unknown>;
}

let adminSessionProbe: Promise<boolean> | null = null;

/**
 * Check whether the stored admin token still authenticates against
 * /api/v1/status. Concurrent 401 handlers share one in-flight probe.
 */
function probeAdminSession(): Promise<boolean> {
  if (!adminSessionProbe) {
    adminSessionProbe = (async () => {
      try {
        const headers: Record<string, string> = {};
        const token = getAdminToken();
        if (token) headers["Authorization"] = `Bearer ${token}`;
        const resp = await fetch("/api/v1/status", { headers });
        return resp.ok;
      } catch {
        return false;
      } finally {
        adminSessionProbe = null;
      }
    })();
  }
  return adminSessionProbe;
}

/** Query string for the stats endpoints: rolling `hours`, or a custom `startAt`/`endAt` range (epoch ms). */
function statsQuery(args?: Record<string, unknown>, defaultHours?: number): string {
  const params = new URLSearchParams();
  const custom = args?.startAt != null || args?.endAt != null;
  const hours = args?.hours ?? (custom ? undefined : defaultHours);
  if (hours != null) params.set("hours", String(hours));
  if (args?.startAt != null) params.set("start_at", String(args.startAt));
  if (args?.endAt != null) params.set("end_at", String(args.endAt));
  const qs = params.toString();
  return qs ? `?${qs}` : "";
}

function resolveHTTP(cmd: string, args?: Record<string, unknown>): HTTPMapping {
  const base = "/api/v1";
  switch (cmd) {
    case "get_providers":
      return { method: "GET", url: `${base}/providers` };
    case "get_provider_presets":
      return { method: "GET", url: `${base}/providers/presets` };
    case "get_loaded_extensions":
      return { method: "GET", url: `${base}/system/extensions` };
    case "create_provider":
      return { method: "POST", url: `${base}/providers`, body: args?.input as Record<string, unknown> };
    case "copy_provider":
      return {
        method: "POST",
        url: `${base}/providers/${args?.id}/copy`,
        body: (args?.options ?? {}) as Record<string, unknown>,
      };
    case "update_provider":
      return { method: "PUT", url: `${base}/providers/${args?.id}`, body: args?.input as Record<string, unknown> };
    case "delete_provider":
      return { method: "DELETE", url: `${base}/providers/${args?.id}` };
    case "test_provider":
      return { method: "GET", url: `${base}/providers/${args?.id}/test` };
    case "get_provider_usage":
      return { method: "GET", url: `${base}/providers/${args?.id}/usage` };
    case "list_provider_usage":
      return { method: "GET", url: `${base}/providers/usage` };
    case "get_provider_usage_credentials":
      return { method: "GET", url: `${base}/providers/${args?.id}/usage-credentials` };
    case "set_provider_usage_credentials":
      return {
        method: "PUT",
        url: `${base}/providers/${args?.id}/usage-credentials`,
        body: {
          access_key: String(args?.access_key ?? ""),
          secret_key: String(args?.secret_key ?? ""),
        },
      };
    case "test_provider_models":
      return { method: "GET", url: `${base}/providers/${args?.id}/test-models` };
    case "probe_provider_keys":
      return { method: "POST", url: `${base}/providers/${args?.id}/probe-keys` };
    case "probe_provider_models":
      return {
        method: "POST",
        url: `${base}/providers/${args?.id}/probe-models`,
        // Omit the body entirely for legacy full runs; an explicit empty list
        // is rejected server-side, so only send real selections.
        ...(Array.isArray(args?.models) ? { body: { models: args.models } } : {}),
      };
    case "get_provider_models":
      return { method: "GET", url: `${base}/providers/${args?.id}/models${args?.requireCatalog === true ? "?require_catalog=true" : ""}` };
    case "get_model_performance": {
      const query = args?.providerId == null ? "" : `?provider_id=${encodeURIComponent(String(args.providerId))}`;
      return { method: "GET", url: `${base}/model-performance${query}` };
    }
    case "list_model_ratings":
      return { method: "GET", url: `${base}/model-ratings` };
    case "set_model_rating":
    case "delete_model_rating":
      return {
        method: cmd === "set_model_rating" ? "PUT" : "DELETE",
        url: `${base}/model-ratings?prefix=${encodeURIComponent(String(args?.modelPrefix ?? ""))}`,
        ...(cmd === "set_model_rating" ? { body: args?.input as Record<string, unknown> } : {}),
      };
    case "get_model_capabilities":
      return {
        method: "GET",
        url: `${base}/providers/${args?.providerId}/model-capabilities?model=${encodeURIComponent(String(args?.model ?? ""))}`,
      };
    case "get_provider_account_pool":
      return { method: "GET", url: `${base}/providers/${encodeURIComponent(String(args?.id))}/accounts` };
    case "get_provider_account_usage_stats":
      return { method: "GET", url: `${base}/providers/${encodeURIComponent(String(args?.id))}/accounts/usage-stats?period_start=${encodeURIComponent(String(args?.periodStart))}` };
    case "configure_provider_account_pool":
      return { method: "PUT", url: `${base}/providers/${encodeURIComponent(String(args?.id))}/account-pool`, body: args?.config as Record<string, unknown> };
    case "init_provider_account_session":
      return {
        method: "POST", url: `${base}/providers/${encodeURIComponent(String(args?.id))}/accounts/auth-sessions`,
        body: { account_id: args?.accountId ?? null, expected_version: args?.expectedVersion ?? null, kiro: args?.kiro },
      };
    case "bind_provider_account":
      return {
        method: "POST", url: `${base}/providers/${encodeURIComponent(String(args?.id))}/accounts/bind`,
        body: { session_id: args?.sessionId, label: args?.label ?? null },
      };
    case "update_provider_account":
      return {
        method: "PATCH", url: `${base}/providers/${encodeURIComponent(String(args?.id))}/accounts/${encodeURIComponent(String(args?.accountId))}`,
        body: { expected_version: args?.expectedVersion, update: args?.update },
      };
    case "disconnect_provider_account":
      return {
        method: "DELETE", url: `${base}/providers/${encodeURIComponent(String(args?.id))}/accounts/${encodeURIComponent(String(args?.accountId))}`,
        body: { expected_version: args?.expectedVersion },
      };
    case "refresh_provider_account_usage":
      return { method: "POST", url: `${base}/providers/${encodeURIComponent(String(args?.id))}/accounts/${encodeURIComponent(String(args?.accountId))}/usage/refresh` };
    case "refresh_provider_account_pool_usage":
      return { method: "POST", url: `${base}/providers/${encodeURIComponent(String(args?.id))}/account-pool/usage/refresh` };
    case "get_provider_oauth_status":
      return { method: "GET", url: `${base}/providers/${args?.id}/oauth/status` };
    case "bind_provider_oauth":
      return {
        method: "POST",
        url: `${base}/providers/${args?.id}/oauth/bind`,
        body: { session_id: args?.sessionId ?? args?.session_id },
      };
    case "reconnect_provider_oauth":
      return { method: "POST", url: `${base}/providers/${args?.id}/oauth/reconnect` };
    case "logout_provider_oauth":
      return { method: "POST", url: `${base}/providers/${args?.id}/oauth/logout` };
    case "init_oauth_session":
      return {
        method: "POST",
        url: `${base}/oauth/sessions/init`,
        body: {
          vendor: args?.vendor,
          use_proxy: args?.useProxy ?? args?.use_proxy,
          kiro: args?.kiro,
        },
      };
    case "get_oauth_session_status":
      return { method: "GET", url: `${base}/oauth/sessions/${args?.sessionId ?? args?.session_id}/status` };
    case "cancel_oauth_session":
      return { method: "POST", url: `${base}/oauth/sessions/${args?.sessionId ?? args?.session_id}/cancel` };
    case "complete_oauth_session":
      return {
        method: "POST",
        url: `${base}/oauth/sessions/${args?.sessionId ?? args?.session_id}/complete`,
        body: {
          code: args?.code,
          callback_url: args?.callbackUrl ?? args?.callback_url,
          metadata: args?.metadata ?? {},
        },
      };
    case "create_devin_provider_with_pool":
      return {
        method: "POST", url: `${base}/providers/devin`,
        body: { session_id: args?.sessionId, input: args?.input, config: args?.config },
      };
    case "create_oauth_provider":
      return {
        method: "POST",
        url: `${base}/providers/oauth`,
        body: {
          session_id: args?.sessionId ?? args?.session_id,
          input: args?.input,
        },
      };
    case "list_models":
      return { method: "GET", url: `${base}/models` };
    case "create_model":
      return { method: "POST", url: `${base}/models`, body: args?.input as Record<string, unknown> };
    case "update_model":
      return { method: "PUT", url: `${base}/models/${args?.id}`, body: args?.input as Record<string, unknown> };
    case "delete_model":
      return { method: "DELETE", url: `${base}/models/${args?.id}` };

    case "list_api_keys":
      return { method: "GET", url: `${base}/api-keys` };
    case "create_api_key":
      return { method: "POST", url: `${base}/api-keys`, body: args?.input as Record<string, unknown> };
    case "update_api_key":
      return { method: "PUT", url: `${base}/api-keys/${args?.id}`, body: args?.input as Record<string, unknown> };
    case "delete_api_key":
      return { method: "DELETE", url: `${base}/api-keys/${args?.id}` };

    case "query_logs": {
      const q = (args?.query ?? {}) as Record<string, unknown>;
      const params = new URLSearchParams();
      for (const [k, v] of Object.entries(q)) {
        if (v != null) params.set(k, String(v));
      }
      const qs = params.toString();
      return { method: "GET", url: `${base}/logs${qs ? "?" + qs : ""}` };
    }
    case "get_log":
      return { method: "GET", url: `${base}/logs/${args?.id}` };
    case "get_request_log_attempts":
      return { method: "GET", url: `${base}/log-requests/${encodeURIComponent(String(args?.requestId ?? ""))}` };
    case "get_logging_status":
      return { method: "GET", url: `${base}/logging/status` };
    case "clear_logs":
      return { method: "DELETE", url: `${base}/logs` };
    case "clear_log_payloads":
      return { method: "DELETE", url: `${base}/logs/payloads` };
    case "clear_error_logs":
      return { method: "DELETE", url: `${base}/logs?scope=errors` };
    case "delete_log":
      return { method: "DELETE", url: `${base}/logs/${args?.id}` };

    case "get_token_price": {
      const params = new URLSearchParams({ provider_id: String(args?.providerId ?? ""), model: String(args?.model ?? "") });
      return { method: "GET", url: `${base}/stats/token-price?${params.toString()}` };
    }
    case "set_token_price":
      return { method: "PUT", url: `${base}/stats/token-price`, body: args?.input as Record<string, unknown> };
    case "get_token_usage_stats": {
      const params = new URLSearchParams({ year: String(args?.year ?? new Date().getUTCFullYear()) });
      return { method: "GET", url: `${base}/stats/token-usage?${params.toString()}` };
    }
    case "get_stats_overview": {
      return {
        method: "GET",
        url: `${base}/stats/overview${statsQuery(args)}`,
      };
    }
    case "get_stats_hourly": {
      const hours = args?.hours ?? 24;
      return { method: "GET", url: `${base}/stats/hourly?hours=${hours}` };
    }
    case "get_stats_timeseries": {
      return { method: "GET", url: `${base}/stats/timeseries${statsQuery(args, 24)}` };
    }
    case "get_stats_by_model": {
      return {
        method: "GET",
        url: `${base}/stats/models${statsQuery(args)}`,
      };
    }
    case "get_model_usage_stats": {
      const params = new URLSearchParams({ model: String(args?.model ?? "") });
      return {
        method: "GET",
        url: `${base}/providers/${args?.providerId}/model-usage?${params.toString()}`,
      };
    }
    case "get_stats_by_provider": {
      return {
        method: "GET",
        url: `${base}/stats/providers${statsQuery(args)}`,
      };
    }
    case "get_provider_usage_detail": {
      return {
        method: "GET",
        url: `${base}/stats/providers/${encodeURIComponent(String(args?.id ?? ""))}${statsQuery(args)}`,
      };
    }
    case "get_stats_by_api_key": {
      return {
        method: "GET",
        url: `${base}/stats/api-keys${statsQuery(args)}`,
      };
    }
    case "get_api_key_usage_detail": {
      return {
        method: "GET",
        url: `${base}/stats/api-keys/${encodeURIComponent(String(args?.id ?? ""))}${statsQuery(args)}`,
      };
    }
    case "get_model_usage_detail": {
      return {
        method: "GET",
        url: `${base}/stats/models/${encodeURIComponent(String(args?.model ?? ""))}${statsQuery(args)}`,
      };
    }

    case "get_setting":
      return { method: "GET", url: `${base}/settings/${args?.key}` };
    case "set_setting":
      return { method: "PUT", url: `${base}/settings/${args?.key}`, body: { value: args?.value } };

    case "get_gateway_status":
      return { method: "GET", url: `${base}/status` };

    case "export_config":
      return { method: "GET", url: `${base}/config/export` };
    case "import_config":
      return { method: "POST", url: `${base}/config/import`, body: args?.data as Record<string, unknown> };

    default:
      return { method: "POST", url: `${base}/${cmd}`, body: args };
  }
}

export const backend = IS_TAURI ? invokeIPC : invokeHTTP;
export { IS_TAURI };