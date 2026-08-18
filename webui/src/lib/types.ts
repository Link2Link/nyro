export interface Provider {
  id: string;
  name: string;
  vendor?: string | null;
  protocol: string;
  base_url: string;
  protocol_mode?: ProviderProtocolMode;
  protocol_endpoints?: ProviderProtocolEndpoint[];
  api_key?: string;
  use_proxy: boolean;
  auth_mode?: "apikey" | "oauth";
  oauth_status?: ProviderOAuthStatus;
  oauth_expires_at?: string | null;
  oauth_last_error?: string | null;
  oauth_updated_at?: string | null;
  preset_key?: string | null;
  channel?: string | null;
  models_source?: string | null;
  static_models?: string | null;
  last_test_success?: boolean | null;
  last_test_at?: string | null;
  is_enabled: boolean;
  created_at: string;
  updated_at: string;
}

export interface Model {
  id: string;
  name: string;
  balance: ModelBalance;
  target_provider: string;
  target_model: string;
  enable_auth: boolean;
  enable_payload?: boolean | null;
  is_enabled: boolean;
  created_at: string;
  targets: ModelBackend[];
}

export type ModelBalance = "weighted" | "priority";

export interface ModelBackend {
  id: string;
  model_id: string;
  provider_id: string;
  model: string;
  weight: number;
  priority: number;
  created_at: string;
}

export interface ApiKey {
  id: string;
  key: string;
  name: string;
  rpm?: number | null;
  rpd?: number | null;
  tpm?: number | null;
  tpd?: number | null;
  is_enabled: boolean;
  expires_at?: string | null;
  created_at: string;
  updated_at: string;
  model_ids: string[];
}

export interface RequestLog {
  id: string;
  /** Unix 毫秒时间戳 */
  created_at: number;
  api_key_id?: string;
  api_key_name?: string;

  client_protocol?: string;
  upstream_protocol?: string;
  provider_id?: string;
  provider_name?: string;
  model_id?: string;
  model_name?: string;
  upstream_url?: string;
  client_model?: string;
  upstream_model?: string;

  /** 客户端请求的归一化推理强度（"high" 等定性值或 "budget:<n>"），未声明时缺省 */
  reasoning_effort?: string | null;

  method?: string;
  path?: string;

  client_request_headers?: string;
  client_request_body?: string;
  client_response_headers?: string;
  client_response_body?: string;

  upstream_request_headers?: string;
  upstream_request_body?: string;
  upstream_response_headers?: string;
  upstream_response_body?: string;

  upstream_status_code?: number;
  client_status_code?: number;

  latency_total_ms?: number;
  latency_upstream_ms?: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens?: number;

  is_stream: boolean;
  stream_chunks_count: number;
  stream_first_chunk_ms?: number;
}

export function getRouteType(log: Pick<RequestLog, "path">): "chat" | "embedding" {
  return log.path === "/v1/embeddings" ? "embedding" : "chat";
}

export interface LogPage {
  items: RequestLog[];
  total: number;
}

export interface GatewayStatus {
  status: string;
  proxy_host?: string;
  proxy_port: number;
}

export interface StatsOverview {
  total_requests: number;
  total_input_tokens: number;
  total_output_tokens: number;
  total_cache_read_tokens: number;
  avg_duration_ms: number;
  error_count: number;
}

export interface StatsHourly {
  hour: string;
  request_count: number;
  error_count: number;
  total_input_tokens: number;
  total_output_tokens: number;
  total_cache_read_tokens: number;
  avg_duration_ms: number;
}

export interface StatsTimeBucket {
  bucket_start: number;
  request_count: number;
  error_count: number;
  total_input_tokens: number;
  total_output_tokens: number;
  total_cache_read_tokens: number;
  avg_duration_ms: number | null;
}

export interface StatsTimeSeries {
  start_at: number;
  end_at: number;
  bucket_minutes: number;
  has_data: boolean;
  points: StatsTimeBucket[];
}

export interface ModelStats {
  model: string;
  request_count: number;
  total_input_tokens: number;
  total_output_tokens: number;
  total_cache_read_tokens: number;
  avg_duration_ms: number;
  total_upstream_ms: number;
}

export interface ModelUsageStats {
  request_count: number;
  total_input_tokens: number;
  total_output_tokens: number;
  total_cache_read_tokens: number;
  last_called_at?: number | null;
  recent_sample_count: number;
  average_tps?: number | null;
  average_first_token_ms?: number | null;
}

export interface ProviderStats {
  provider: string;
  request_count: number;
  error_count: number;
  avg_duration_ms: number;
  total_output_tokens: number;
  total_upstream_ms: number;
}

export interface ApiKeyStats {
  api_key_id: string;
  api_key_name: string;
  request_count: number;
  error_count: number;
  total_input_tokens: number;
  total_output_tokens: number;
  cache_read_tokens: number;
  last_used_at: number;
}

export interface TestResult {
  success: boolean;
  latency_ms: number;
  model?: string;
  error?: string;
  endpoints?: EndpointTestResult[];
}

export interface EndpointTestResult {
  endpoint_id: string;
  protocol: string;
  base_url: string;
  success: boolean;
  latency_ms: number;
  error?: string;
  tested_at: string;
}

export interface ModelCapabilities {
  provider: string;
  model_id: string;
  context_window: number;
  embedding_length?: number | null;
  output_max_tokens?: number | null;
  input_cost?: number | null;
  output_cost?: number | null;
  tool_call: boolean;
  reasoning: boolean;
  input_modalities: string[];
  output_modalities: string[];
}

export type ProviderProtocol =
  | "openai-compatible"
  | "openai-responses"
  | "anthropic-messages"
  | "google-gemini";

export type ProviderProtocolMode = "fixed" | "adaptive";

export type ProviderEndpointProtocol =
  | "openai-compatible/chat-completions/v1"
  | "openai-compatible/embeddings/v1"
  | "openai-responses/responses/v1"
  | "anthropic-messages/messages/2023-06-01"
  | "google-gemini/generate-content/v1beta";

export type ProviderEndpointAuthScheme = "auto" | "bearer" | "x-api-key" | "query" | "none";

export interface CreateProviderProtocolEndpoint {
  protocol: string;
  base_url: string;
  api_key: string;
  auth_scheme?: ProviderEndpointAuthScheme;
  is_enabled?: boolean;
  priority?: number;
}

export interface ProviderProtocolEndpoint extends CreateProviderProtocolEndpoint {
  id: string;
  provider_id: string;
  auth_scheme: ProviderEndpointAuthScheme;
  is_enabled: boolean;
  priority: number;
  test_status: "untested" | "success" | "failed" | string;
  test_error?: string | null;
  tested_at?: string | null;
  created_at: string;
  updated_at: string;
}

export interface ProviderChannelPreset {
  id: string;
  label: {
    zh: string;
    en: string;
  };
  authMode?: "apikey" | "oauth";
  baseUrls: Record<string, string>;
  modelsSource?: string;
  apiKey?: string;
  modelsEndpoint?: string;
  staticModels?: string[];
  /** One API key is valid for every protocol in `baseUrls`; the form renders a single key field plus protocol checkboxes. */
  sharedKeyProtocols?: boolean;
  /** Per-protocol auth-scheme overrides (e.g. `{ "anthropic-messages": "bearer" }`); absent protocol = "auto". */
  authSchemes?: Record<string, string>;
}

/** Per-model result of the "send hi" probe over a provider's model list. */
export interface ModelProbeResult {
  model: string;
  success: boolean;
  error?: string | null;
  latency_ms: number;
  /** Canonical protocol endpoint id used for the probe. */
  protocol: string;
  /** Assistant text received for the "hi" probe (success only). */
  reply?: string | null;
}

/** Which protocol/base_url a model probe ran through. */
export interface ModelProbeMeta {
  protocol: string;
  base_url: string;
}

/** Full model-probe response: shared run metadata plus per-model results. */
export interface ModelProbeOutcome {
  meta: ModelProbeMeta;
  results: ModelProbeResult[];
}

/** Usage-query credentials for a provider (Ark IAM AK/SK, DeepSeek platform token, ...). */
export interface ProviderUsageCredentials {
  access_key?: string | null;
  secret_key?: string | null;
}

/** A spend figure over a period (e.g. DeepSeek today/month cost). */
export interface ProviderUsageSpend {
  /** `today` | `month` */
  name: string;
  /** Consumed amount in the account currency. */
  amount: number;
  /** ISO currency code, e.g. `CNY`. */
  currency: string;
}

export interface ProviderPreset {
  id: string;
  label: {
    zh: string;
    en: string;
  };
  icon?: string;
  defaultProtocol: string;
  channels?: ProviderChannelPreset[];
}

export interface CreateProvider {
  name: string;
  vendor?: string;
  protocol: string;
  base_url: string;
  protocol_mode?: ProviderProtocolMode;
  protocol_endpoints?: CreateProviderProtocolEndpoint[];
  use_proxy?: boolean;
  auth_mode?: "apikey" | "oauth";
  preset_key?: string;
  channel?: string;
  models_source?: string;
  static_models?: string;
  api_key: string;
}

export interface UpdateProvider {
  name?: string;
  vendor?: string;
  protocol?: string;
  base_url?: string;
  protocol_mode?: ProviderProtocolMode;
  protocol_endpoints?: CreateProviderProtocolEndpoint[];
  use_proxy?: boolean;
  auth_mode?: "apikey" | "oauth";
  preset_key?: string;
  channel?: string;
  models_source?: string;
  static_models?: string;
  api_key?: string;
  is_enabled?: boolean;
}

export interface CreateModel {
  name: string;
  balance?: ModelBalance;
  target_provider: string;
  target_model: string;
  targets?: CreateModelBackend[];
  enable_auth?: boolean;
  enable_payload?: boolean | null;
}

export interface UpdateModel {
  name?: string;
  balance?: ModelBalance;
  target_provider?: string;
  target_model?: string;
  targets?: UpsertModelBackend[];
  enable_auth?: boolean;
  enable_payload?: boolean | null;
  is_enabled?: boolean;
}

export interface CreateModelBackend {
  provider_id: string;
  model: string;
  weight?: number;
  priority?: number;
}

export interface UpsertModelBackend {
  id?: string;
  provider_id: string;
  model: string;
  weight?: number;
  priority?: number;
}

export interface CreateApiKey {
  name: string;
  rpm?: number;
  rpd?: number;
  tpm?: number;
  tpd?: number;
  expires_at?: string;
  model_ids: string[];
}

export interface UpdateApiKey {
  name?: string;
  rpm?: number;
  rpd?: number;
  tpm?: number;
  tpd?: number;
  is_enabled?: boolean;
  expires_at?: string;
  model_ids?: string[];
}

export interface LogQuery {
  limit?: number;
  offset?: number;
  provider?: string;
  model?: string;
  status_min?: number;
  status_max?: number;
  api_key?: string;
  /** 起始时间(Unix 毫秒)，筛选 created_at >= after */
  after?: number;
  /** 结束时间(Unix 毫秒)，筛选 created_at <= before */
  before?: number;
}

export interface ExportData {
  version: number;
  providers: ExportProvider[];
  models: ExportModel[];
  settings: [string, string][];
}

export interface ExportProvider {
  name: string;
  vendor?: string | null;
  protocol: string;
  base_url: string;
  use_proxy: boolean;
  preset_key?: string | null;
  channel?: string | null;
  models_source?: string | null;
  static_models?: string | null;
  api_key: string;
  is_enabled: boolean;
}

export interface ExportModel {
  name: string;
  target_model: string;
  enable_auth: boolean;
  enable_payload?: boolean | null;
  is_enabled: boolean;
}

export interface ImportResult {
  providers_imported: number;
  models_imported: number;
  settings_imported: number;
}


export interface OAuthSessionInitData {
  session_id: string;
  vendor: string;
  scheme: string;
  auth_url: string;
  requires_manual_code: boolean;
  user_code: string;
  verification_uri: string;
  verification_uri_complete: string;
  expires_in: number;
  interval: number;
}

export type OAuthSessionStatusData =
  | {
      status: "pending";
      scheme: string;
      auth_url: string;
      requires_manual_code: boolean;
      expires_in: number;
      interval: number;
      user_code: string;
      verification_uri_complete: string;
    }
  | {
      status: "ready";
      expires_in: number;
      resource_url?: string | null;
    }
  | {
      status: "error";
      code: string;
      message: string;
    };

export type ProviderOAuthStatus =
  | "not_connected"
  | "pending"
  | "connected"
  | "unavailable"
  | "quota_exhausted"
  | "error"
  | "disconnected";

export interface ProviderOAuthStatusData {
  provider_id: string;
  provider_name: string;
  driver_key: string;
  status: ProviderOAuthStatus;
  expires_at?: string | null;
  resource_url?: string | null;
  subject_id?: string | null;
  last_error?: string | null;
  updated_at?: string | null;
  has_refresh_token: boolean;
}

/** Coding-plan usage tier (e.g. GLM 5-hour / weekly quota windows). */
export interface ProviderUsageTier {
  /** `five_hour` | `weekly_limit` */
  name: string;
  /** Used percentage (0-100). */
  used_percent: number;
  /** ISO 8601 reset time when the upstream reports one. */
  resets_at?: string | null;
}

/** Pay-as-you-go account balance, one entry per currency (DeepSeek shape). */
export interface ProviderUsageBalance {
  /** ISO currency code, e.g. `CNY`. */
  currency: string;
  /** Total remaining balance. */
  total: number;
  /** Granted (free/promo) portion of the balance. */
  granted: number;
  /** Topped-up (paid) portion of the balance. */
  topped_up: number;
}

export interface ProviderScheduling {
  status: "eligible" | "quota_exhausted";
  reason?: "usage_limit" | "account_unavailable" | null;
  blocking_tiers: string[];
  reset_at?: string | null;
  next_check_at?: string | null;
}

export interface ProviderUsage {
  provider_id: string;
  /** Query backend kind, e.g. `glm_coding_plan`. */
  kind: string;
  /** Site the quota was read from: `cn` | `global`. */
  site: string;
  /** Plan tier reported by the upstream (e.g. `max`), when present. */
  level?: string | null;
  /** Time-window usage tiers (coding plans). */
  tiers: ProviderUsageTier[];
  /** Account balances (pay-as-you-go vendors). */
  balances?: ProviderUsageBalance[];
  /** Spend figures over periods (e.g. DeepSeek today/month cost). */
  spends?: ProviderUsageSpend[];
  /** Account availability flag when the upstream reports one. */
  is_available?: boolean | null;
  /** Runtime decision controlling whether this provider receives new requests. */
  scheduling?: ProviderScheduling;
  queried_at: string;
}
