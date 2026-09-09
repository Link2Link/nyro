# Database Schema

Nyro supports three storage backends — **SQLite** (default), **PostgreSQL**, and **MySQL** — with the same logical tables. Physical types, collations, defaults, and generated constraint/index names differ; the generated PostgreSQL/MySQL artifacts below record those differences exactly.

## Entity Relationship

```
providers ──1:N── model_backends ──N:1── models
    ├──1:N── provider_protocol_endpoints
    └──1:1── provider_oauth_credentials

model_rating_prefixes (prefix-keyed scores shared across providers; no FK)

api_keys ──M:N── models (via api_key_models)
request_logs (one row per attempt; explicit payload clearing/deletion supported)
    └──N:1── request_results (logical join by client_request_id; no FK)
settings (key-value)
```

---

## providers

AI 模型供应商配置（API endpoint、密钥、认证方式等）。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `name` | TEXT NOT NULL | — | 显示名称 |
| `vendor` | TEXT | NULL | 供应商标识（如 `openai`、`anthropic`） |
| `protocol` | TEXT NOT NULL | — | 默认通信协议；固定模式为协议族，自适应模式为精确端点 ID |
| `base_url` | TEXT NOT NULL | — | 默认端点 Base URL（兼容旧客户端和模型发现） |
| `protocol_mode` | TEXT NOT NULL | `'fixed'` | 协议模式：`fixed` 或 `adaptive` |
| `preset_key` | TEXT | NULL | 预设模板 key（内置供应商模板标识） |
| `channel` | TEXT | NULL | 预设通道 ID（如 `default`、`azure`） |
| `models_source` | TEXT | NULL | 模型列表获取方式 |
| `static_models` | TEXT | NULL | 静态模型列表（`\n` 分隔） |
| `api_key` | TEXT NOT NULL | — | 默认端点 API 密钥（兼容字段） |
| `auth_mode` | TEXT | `'apikey'` | 认证方式：`apikey` 或 `oauth` |
| `access_token` | TEXT | NULL | OAuth access token（迁移至 oauth 表后弃用） |
| `refresh_token` | TEXT | NULL | OAuth refresh token（迁移至 oauth 表后弃用） |
| `expires_at` | TEXT | NULL | OAuth token 过期时间（迁移至 oauth 表后弃用） |
| `use_proxy` | INTEGER | `0` | 是否通过代理发送请求 |
| `fast_mode` | INTEGER | `0` | sub2api 渠道 Fast 模式：开启后 OpenAI Responses 上游请求自动附加 `service_tier=priority`（客户端显式指定时优先保留） |
| `last_test_success` | INTEGER | NULL | 最近一次连通性测试是否成功 |
| `last_test_at` | TEXT | NULL | 最近一次连通性测试时间 |
| `is_enabled` | INTEGER | `1` | 是否启用 |
| `priority` | INTEGER | `0` | 优先级（预留） |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |
| `updated_at` | TEXT | `datetime('now')` | 更新时间 |

---

## provider_protocol_endpoints

Provider 的协议端点明细。固定模式保留一条兼容记录；自适应模式可配置多个精确协议端点，每个端点独立保存 Base URL、凭据和认证方式。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `provider_id` | TEXT NOT NULL | — | 所属 Provider（FK → providers.id, ON DELETE CASCADE） |
| `protocol` | TEXT NOT NULL | — | 精确协议端点 ID，如 `openai-compatible/chat-completions/v1` |
| `base_url` | TEXT NOT NULL | — | 该协议端点的 Base URL |
| `api_key` | TEXT NOT NULL | — | 该协议端点的 API Key |
| `auth_scheme` | TEXT NOT NULL | `'auto'` | `auto`、`bearer`、`x-api-key`、`query` 或 `none` |
| `is_enabled` | INTEGER | `1` | 是否参与协议匹配 |
| `priority` | INTEGER | `0` | 稳定显示与默认排序顺序 |
| `test_status` | TEXT NOT NULL | `'untested'` | 最近测试状态：`untested`、`success` 或 `failed` |
| `test_error` | TEXT | NULL | 最近一次测试错误 |
| `tested_at` | TEXT | NULL | 最近一次测试时间 |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |
| `updated_at` | TEXT | `datetime('now')` | 更新时间 |

**唯一约束**：`(provider_id, protocol)`

**索引**：`idx_provider_protocol_endpoints_provider` on `(provider_id, is_enabled, priority)`

---

## model_rating_prefixes

The latest manually assigned integer comprehensive score for a **model-name prefix**, shared by every provider whose upstream model equals the prefix or continues after it at a `-` segment boundary (longest matching prefix wins). Matching and identity are case-insensitive: the admin layer canonicalizes prefixes to lowercase on write, so case variants can never become two same-length entries. Zero is a real rating and absence means unrated. Entries are provider-independent administration data: deleting a provider or route never deletes them. Ratings do not affect routing and do not distinguish reasoning effort. Legacy provider-scoped `provider_model_ratings` rows were deliberately dropped during migration instead of converted.

| Column | Type | Default | Description |
|---|---|---|---|
| `model_prefix` | TEXT NOT NULL (MySQL: VARBINARY(1024)) | — | User-chosen model prefix, stored lowercased (case-insensitive matching); otherwise byte-exact, including trailing spaces; 1–1024 UTF-8 bytes |
| `score` | INTEGER NOT NULL | — | Integer **0–100 inclusive**, database `CHECK` constraint; zero is a real rating, not “unrated” |
| `updated_at` | TEXT NOT NULL | — | Application-written UTC RFC3339 timestamp with millisecond precision, e.g. `2026-09-08T02:30:45.123Z` |

**Physical primary key**: `(model_prefix)`. The API/service validates the 1024-UTF-8-byte limit consistently for all backends (bytes, not character count). Export/import use a flat top-level `model_ratings` array of `{model_prefix, score, updated_at}`; old backups with per-provider nested ratings import providers but ignore the nested scores.

**Physical identity and validation**:
- SQLite uses `TEXT COLLATE BINARY`, a byte-length check via `length(CAST(model_prefix AS BLOB))`, and `typeof(score) = 'integer'` plus the range check (SQLite's type affinity alone is not an integer constraint).
- PostgreSQL uses `TEXT COLLATE "C"`, `octet_length` for the prefix byte-length check, and the native `INTEGER` type plus the range check.
- MySQL uses `VARBINARY(1024)`, storing the original UTF-8 bytes rather than a case-folding or trailing-space-insensitive text collation. `OCTET_LENGTH` enforces a nonempty prefix; the binary column bounds its maximum size. Invalid UTF-8 bytes surface as an error on read, never as replacement text. Score is native `INTEGER` plus the range check (requires MySQL 8.0.16+ for enforced `CHECK`s).

---

## models

虚拟模型配置，定义客户端请求的模型名如何映射到后端。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `name` | TEXT NOT NULL | — | 显示名称，同时作为客户端请求的模型匹配键（路由唯一键的一部分） |
| `balance` | TEXT | `'weighted'` | 多后端负载均衡策略：`weighted`、`priority`、`latency`、`usage`（按最大周期窗口剩余额度÷剩余时间比率 r³，再乘窗口易损性加成 `(30d/W)^0.5`（月=1、周≈2.07、5h 封顶 4）动态加权） |
| `target_provider` | TEXT NOT NULL | — | 默认后端 provider ID（FK → providers.id） |
| `target_model` | TEXT NOT NULL | — | 默认后端使用的上游模型名 |
| `enable_auth` | INTEGER | `0` | 是否启用 API Key 访问控制 |
| `enable_payload` | INTEGER | — | 是否记录载荷（headers/bodies）。NULL = 默认记录（受全局 `enable_payload` 开关控制） |
| `force_max_reasoning` | INTEGER NOT NULL | `0` | 模型映射级「max推理」覆盖：`1` = 该路由的所有请求强制按 max 推理档出站（客户端显式档位、未携带推理指令、关闭与预算制均被覆盖；vendor 出站方言仍照常裁决，如 grok max→xhigh）。转码/compat 路径在 IR 层改写，原生直通路径在 wire 层改写以保持逐字保真 |
| `vision_shim` | TEXT | — | 视觉垫片配置 JSON（`VisionShimConfig`）。配置 helper 后该路由启用多模态门面：请求中的图片先由 helper 模型转录为文字再出站。`helper_backends` 为 provider+模型对列表（跨供应商、按序故障转移）；旧式单字段 `helper_model`(+`helper_provider`) 仍兼容。空对象 `{}` 表示清除 |
| `is_enabled` | INTEGER | `1` | 是否启用 |
| `priority` | INTEGER | `0` | 优先级（预留） |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |

---

## model_backends

模型后端列表，一个 model 可对应多个 provider + 上游模型的组合。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `model_id` | TEXT NOT NULL | — | 所属模型 ID（FK → models.id, ON DELETE CASCADE） |
| `provider_id` | TEXT NOT NULL | — | 供应商 ID（FK → providers.id） |
| `model` | TEXT NOT NULL | — | 上游模型名（发送给 provider 的模型标识） |
| `weight` | INTEGER | `100` | 静态权重（`weighted` 使用；`usage` 中用于同 Provider 多 target 的内部顺序及未知用量兜底） |
| `priority` | INTEGER | `1` | 优先级，数值越小越优先（`priority` 策略下生效） |
| `is_fallback` | INTEGER NOT NULL | `0` | 降级兜底行：不参与任何 balance 策略，仅追加在有序目标列表末尾——所有正常目标被跳过（配额/熔断/禁用）或可重试失败后才会调用。每个模型至多一行，且不能是唯一一行 |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |

**索引**：`idx_model_backends_model_id` on `model_id`

---

## api_keys

API 密钥管理，用于代理端口的访问认证和限流。

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | 主键，UUID |
| `token` | TEXT NOT NULL UNIQUE | — | 密钥值（如 `nyro-xxxx`） |
| `name` | TEXT NOT NULL | — | 显示名称 |
| `rpm` | INTEGER | NULL | 每分钟请求数限制 |
| `rpd` | INTEGER | NULL | 每日请求数限制 |
| `tpm` | INTEGER | NULL | 每分钟 token 数限制 |
| `tpd` | INTEGER | NULL | 每日 token 数限制 |
| `is_enabled` | INTEGER | `1` | 是否启用 |
| `is_privileged` | INTEGER NOT NULL | `0` | 特权秘钥：跳过模型绑定检查，可访问所有开启访问控制的模型（启用/过期/限流仍生效）；已有绑定保留不动 |
| `expires_at` | TEXT | NULL | 过期时间 |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |
| `updated_at` | TEXT | `datetime('now')` | 更新时间 |

**索引**：`idx_api_keys_token` on `token`

---

## api_key_models

API Key 与模型的访问绑定关系（M:N 关联表）。仅当 model 启用 `enable_auth` 时生效。

| Column | Type | Description |
|---|---|---|
| `api_key_id` | TEXT NOT NULL | API Key ID（FK → api_keys.id, ON DELETE CASCADE） |
| `model_id` | TEXT NOT NULL | 模型 ID（FK → models.id, ON DELETE CASCADE） |

**主键**：`(api_key_id, model_id)`

**索引**：`idx_api_key_models_model_id` on `model_id`

---

## provider_oauth_credentials

OAuth 凭据存储，用于需要 OAuth 认证的供应商（如 Google Vertex AI）。

| Column | Type | Default | Description |
|---|---|---|---|
| `provider_id` | TEXT PK | — | 供应商 ID（FK → providers.id, ON DELETE CASCADE） |
| `driver_key` | TEXT | `''` | OAuth 驱动标识 |
| `scheme` | TEXT | `''` | 认证方案 |
| `access_token` | TEXT | `''` | OAuth access token |
| `refresh_token` | TEXT | NULL | OAuth refresh token |
| `expires_at` | TEXT | NULL | Token 过期时间 |
| `resource_url` | TEXT | NULL | 资源 URL（部分 OAuth 流程需要） |
| `subject_id` | TEXT | NULL | 认证主体 ID |
| `scopes` | TEXT | `'[]'` | OAuth 权限范围（JSON 数组） |
| `meta` | TEXT | `'{}'` | 扩展元数据（JSON） |
| `status` | TEXT | `'connected'` | 连接状态 |
| `status_version` | INTEGER | `0` | 状态版本号（乐观锁） |
| `last_error` | TEXT | NULL | 最近一次错误信息 |
| `last_refresh_at` | TEXT | NULL | 最近一次 token 刷新时间 |
| `created_at` | TEXT | `datetime('now')` | 创建时间 |
| `updated_at` | TEXT | `datetime('now')` | 更新时间 |

---

## request_logs

One diagnostic row per gateway attempt, not necessarily one per client request.
Captured content may be absent, partial, truncated, or explicitly cleared; the log
is not a promise of a complete wire transcript. Final client-request results live
separately in `request_results` and do not add to usage counts. See
[failure observability](../design/failure-observability.md)
([中文](../design/failure-observability_CN.md)) for classification, capture, privacy,
and loss-accounting semantics.

| Column | Type | Default | Description |
|---|---|---|---|
| `id` | TEXT PK | — | Stable log ID allocated before persistence, preserved across fallback/builder copies |
| `client_request_id` | TEXT (MySQL: VARCHAR(64), ASCII binary collation) | NULL | Correlation ID shared by attempts of one client request; legacy rows may lack it |
| `attempt_index` | INTEGER | NULL | Upstream attempt index within the client request; pre-upstream diagnostics may use 0 |
| `outcome_version` | INTEGER NOT NULL | `0` | Authority version independent of performance metadata; only version 1 is currently recognized; no historical promotion |
| `attempt_outcome` | TEXT NOT NULL (MySQL: VARCHAR(32), ASCII binary collation) | `'unknown'` | Versioned observation: `completed`, `failed`, `timed_out`, `cancelled`, `output_limited`, or `unknown`; unknown versions/values are not authoritative |
| `failure_kind` | TEXT (MySQL: VARCHAR(64)) | NULL | Bounded diagnostic category, such as a transport, parse, conversion, timeout, or output-limit condition |
| `failure_stage` | TEXT (MySQL: VARCHAR(64)) | NULL | Bounded stage identifying where failure was observed |
| `error_message` | TEXT | NULL | Bounded sanitized summary, not raw body content |
| `error_causes_json` | TEXT | NULL | JSON-encoded bounded sanitized cause array; public `RequestLog.error_causes` is a JSON string |
| `payload_metadata_json` | TEXT | NULL | JSON-encoded per-header/body capture metadata; public `RequestLog.payload_metadata` is a JSON string, not a second body copy |
| `payload_cleared_at` | INTEGER (PostgreSQL/MySQL: BIGINT) | NULL | Unix millisecond timestamp when an explicit admin clear removed payloads; classification/capture metadata remains |
| `created_at` | INTEGER | `0` | Unix 毫秒时间戳 |
| `api_key_id` | TEXT | NULL | 认证使用的 API Key ID |
| `api_key_name` | TEXT | NULL | API Key 名称（快照） |
| `client_protocol` | TEXT | NULL | 客户端协议（如 `openai/chat/v1`） |
| `upstream_protocol` | TEXT | NULL | 上游协议 |
| `provider_id` | TEXT | NULL | 供应商 ID |
| `provider_name` | TEXT | NULL | 供应商名称（快照） |
| `model_id` | TEXT | NULL | 匹配到的模型 ID |
| `model_name` | TEXT | NULL | 模型名称（快照） |
| `upstream_url` | TEXT | NULL | 上游请求 URL |
| `client_model` | TEXT | NULL | 客户端请求中的模型名 |
| `upstream_model` | TEXT | NULL | 实际发送给上游的模型名 |
| `reasoning_effort` | TEXT | NULL | 客户端请求的归一化推理强度（`high` 等定性值或 `budget:<n>`；不受载荷记录开关影响） |
| `route_decision` | TEXT | NULL | 路由决策快照 JSON（选择时点采集：全部候选的评分/权重/占比/排序与跳过原因；不受载荷记录开关影响） |
| `method` | TEXT | NULL | HTTP 方法 |
| `path` | TEXT | NULL | 请求路径 |
| `client_request_headers` | TEXT | NULL | 客户端请求头（JSON，可选记录） |
| `client_request_body` | TEXT | NULL | 客户端请求体（可选记录） |
| `client_response_headers` | TEXT | NULL | 客户端响应头（JSON，可选记录） |
| `client_response_body` | TEXT | NULL | 客户端响应体（可选记录） |
| `upstream_request_headers` | TEXT | NULL | 上游请求头（JSON，可选记录） |
| `upstream_request_body` | TEXT | NULL | 上游请求体（可选记录） |
| `upstream_response_headers` | TEXT | NULL | 上游响应头（JSON，可选记录） |
| `upstream_response_body` | TEXT | NULL | 上游响应体（可选记录） |
| `upstream_status_code` | INTEGER | NULL | 上游 HTTP 状态码 |
| `client_status_code` | INTEGER | NULL | 返回给客户端的 HTTP 状态码 |
| `latency_total_ms` | INTEGER | NULL | 总延迟（毫秒） |
| `latency_upstream_ms` | INTEGER | NULL | 上游延迟（毫秒） |
| `input_tokens` | INTEGER | `0` | 输入 token 数 |
| `output_tokens` | INTEGER | `0` | 输出 token 数 |
| `cache_read_tokens` | INTEGER | `0` | 缓存命中 token 数 |
| `is_stream` | INTEGER | `0` | 是否为流式请求 |
| `stream_chunks_count` | INTEGER | `0` | 流式分块数量 |
| `stream_first_chunk_ms` | INTEGER | NULL | 首个分块延迟（毫秒）；旧用量统计保持原口径 |
| `performance_metadata_version` | INTEGER NOT NULL | `0` | Performance evidence format; 0 = legacy/unprocessed, positive versions do not themselves prove completion |
| `upstream_effort_status` | TEXT NOT NULL (MySQL: VARCHAR(16)) | `'unknown'` | `present`, `absent`, or `unknown`, based only on the final outgoing upstream request after rewrites |
| `upstream_effort_raw` | TEXT | NULL | Outgoing effort evidence (including unclassified budget/disabled/other values); never client-effort fallback |
| `upstream_effort_tier` | TEXT (MySQL: VARCHAR(16)) | NULL | Recognized `low`, `medium`, `high`, `xhigh`, `max`; outgoing `minimal` maps to low; other/unspecified effort has no tier |
| `request_completion` | TEXT NOT NULL (MySQL: VARCHAR(16)) | `'unknown'` | Compatibility performance completion evidence: `completed`, `failed`, `cancelled`, `timed_out`, `incomplete`, `output_limited`, or `unknown`; not authority for outcome counts/deletion; historical rows remain unknown |
| `completion_reason` | TEXT | NULL | Observed protocol terminal reason or transport/delivery failure explanation; token-limit outcomes are incomplete, not completed |
| `upstream_response_mode` | TEXT NOT NULL (MySQL: VARCHAR(16)) | `'unknown'` | Observed upstream mode: `stream`, `buffered`, or `unknown`; not inferred from client streaming mode |
| `performance_upstream_ms` | INTEGER (PostgreSQL/MySQL: BIGINT) | NULL | Per-attempt upstream duration in milliseconds, independent of legacy latency fields |
| `performance_first_chunk_ms` | INTEGER (PostgreSQL/MySQL: BIGINT) | NULL | Per-attempt time to first upstream chunk in milliseconds |
| `performance_completed_at` | INTEGER (PostgreSQL/MySQL: BIGINT) | NULL | Unix millisecond completion time, set only for credibly completed attempts |

### Outcome classification and payloads

`RequestLog.is_error`, `effective_outcome`, and optional joined `request_result`
are derived admin fields, **not physical columns**. The shared effective error
predicate is client **or** upstream HTTP **400–599 inclusive**, or version 1
`attempt_outcome IN ('failed', 'timed_out')`. Upstream 4xx/5xx counts even when the
client sees 200. NULL and 600 do not satisfy the HTTP clause. Otherwise only
version 1 can confirm `completed`, `cancelled`, or `output_limited`; every other
row is `unknown`, including legacy `request_completion = 'failed'` without new
authority. HTTP errors remain errors regardless of version.

Usage details expose `outcome_stats_version = 1` and five exclusive counts whose
sum equals total retained attempts: `success_count`, `error_count`,
`cancelled_count`, `output_limited_count`, and `unknown_count`. Success means
confirmed completed, not HTTP 2xx. Success/unknown rates divide by **all attempts**,
not only classified attempts. Final request results are not counted.

Each of the four bodies is bounded to **1 MiB raw bytes** (512 KiB head + 512 KiB
tail); non-UTF-8/split UTF-8 captures use Base64, whose stored string may exceed
that raw-byte cap. Each header block is bounded to **64 KiB serialized JSON**
after credential redaction (a shared **65,535-byte** retained limit fits MySQL
`TEXT` at the boundary). URLs are credential-redacted; body content remains
raw within the cap and is restricted to authorized admin inspection. Metadata
records complete/partial, absent/empty/not-retained, truncation, byte counts,
encoding, and head/tail or omitted-header information. Metadata is retained even
when ordinary payload logging is off.

Confirmed errors, timeouts, cancellations, and output limits force retention of
available bounded evidence. Ordinary completed/unknown attempts use the same caps
but obey both the global and model payload switches. Explicit **Clear payloads**
erases all eight header/body columns for **all** payload-bearing logs, including
errors; it preserves classification and metadata and sets `payload_cleared_at`.
**Clear error logs** uses the same effective error predicate and leaves pure
cancelled/output-limited/unknown rows. Capture metadata after clearing describes
the former capture, not currently available bytes.

### Existing performance evidence

Performance metadata is scalar evidence retained even when payload logging is off.
A per-attempt response Body observer resolves delivery/terminal state **before log
persistence**. Completed means gateway-observed Body EOS plus a successful original
protocol outcome, not a client ACK. Success HTTP status or token usage alone cannot
prove completion. Cancellation, timeout, parse/transport failure, truncation and
length/token-limit outcomes remain distinguishable in this diagnostic metadata,
but completion state is no longer a TPS sampling gate.

**Historical migration is conservative**: every pre-feature row defaults to unknown
completion, regardless of recorded status, usage, terminal marker, or response body.
Bounded recovery may inspect retained final upstream request bodies from the last
7 days to fill effort only; it cannot promote historical completion or use client
`reasoning_effort` as fallback. This diagnostic recovery no longer determines whether
existing requests can populate performance charts.

The Performance query now shares the model-usage TPS helper and samples the latest
ten retained calls for each exact provider/model, ordered by `created_at DESC, id DESC`.
There is no additional seven-day, completion, status, effort, or metadata-version filter.
It reads the existing `output_tokens`, stream/chunk flags, `latency_upstream_ms`,
`latency_total_ms`, and `stream_first_chunk_ms`, not the `performance_*` evidence timings.
Valid per-call TPS values are averaged; invalid samples consume a slot without fetching
older replacements. Sample times are the valid rows' `created_at` timestamps.
The API returns `window_start: null`; compatibility diagnostic counters are zero.
Existing completion/effort metadata remains available for diagnostics only.
Counts and sample times are documented in [model ratings](../design/model-ratings.md#performance-chart).
Existing usage calculations retain their formula; usage and performance now also share
stable ID tie-breaking, null token/chunk handling, and exact MySQL model comparisons.

**索引**：
- `idx_logs_client_request_attempt` on `(client_request_id, attempt_index)`; a non-unique correlation index, not an additional counting identity
- `idx_logs_created_at` on `created_at`
- `idx_logs_provider_id` on `provider_id`
- `idx_logs_client_status` on `client_status_code`
- `idx_logs_upstream_model` on `upstream_model`
- `idx_logs_api_key` on `api_key_id`
- `idx_logs_client_protocol` on `client_protocol`
- `idx_logs_upstream_protocol` on `upstream_protocol`
- `idx_logs_performance_pair` on `(provider_id, upstream_model, request_completion, performance_completed_at, id)`; SQLite uses `upstream_model COLLATE BINARY`, PostgreSQL `COLLATE "C"`; MySQL queries explicitly compare `BINARY upstream_model` for exact identity
- `idx_logs_performance_recovery` on `(performance_metadata_version, created_at, id)` for bounded historical effort-only recovery

---

## request_results

One separately persisted final result per correlated client request. This table
is diagnostic context, **not** an additional attempt/event for request, success,
error, TPS, token, or quota aggregation. Logs remain per-attempt and earlier
failures are not rewritten by eventual success.

| Column | Type | Default | Description |
|---|---|---|---|
| `client_request_id` | TEXT PK (MySQL: VARCHAR(64), ASCII binary collation) | — | Client-request correlation key shared with `request_logs.client_request_id` |
| `final_outcome` | TEXT NOT NULL (MySQL: VARCHAR(32), ASCII binary collation) | — | Final observed request outcome, separate from each attempt's classification |
| `final_attempt_id` | TEXT (MySQL: VARCHAR(36)) | NULL | Stable ID of the final attempt when available; deliberately no FK |
| `attempt_count` | INTEGER NOT NULL | — | Number of upstream attempts recorded by the request lifecycle |
| `finished_at` | INTEGER NOT NULL (PostgreSQL/MySQL: BIGINT) | — | Unix millisecond finalization timestamp |

No foreign key is imposed on the log/result relationship: deleting the final
attempt must not discard useful final context while sibling attempts remain.
An absent final row is not evidence of success. Admin log detail joins this row
as optional `request_result`; historical logs without correlation stay unjoined.
There is no migration backfill of authoritative outcomes or request results from
historical HTTP success/performance markers. The log queue/database write path
can lose evidence and reports explicit process-local loss counters; it has no
retry/spool or historical outcome recovery guarantee.

---

## settings

系统配置键值对。

| Column | Type | Default | Description |
|---|---|---|---|
| `name` | TEXT PK | — | 配置键 |
| `value` | TEXT NOT NULL | — | 配置值 |
| `updated_at` | TEXT | `datetime('now')` | 更新时间 |

---

## Regenerating reference SQL

[`deploy/schema/postgres.sql`](../../deploy/schema/postgres.sql) and
[`deploy/schema/mysql.sql`](../../deploy/schema/mysql.sql) are **derived reference
artifacts**, generated from a real database after core storage `init()` and
`migrate()` have both succeeded. They are not inputs to the generator and must
not be hand-edited. The generator never constructs a `Gateway` or starts
application listeners, seed/config loading, OAuth refresh, or background tasks.

### Prerequisites and safety

1. Build the current tool and storage code: `cargo build -p nyro-tools`.
2. Provision a **new, empty, dedicated disposable database for each run** on a
   temporary local PostgreSQL/MySQL instance (for example a temporary PostgreSQL
   cluster or a disposable Docker container with tmpfs data). Use a role owning
   that database with DDL and metadata-read permissions. Do not use an application,
   production, shared, or persistent database. Ensure no other process uses the
   scratch database during generation: the emptiness check is not a concurrency lock.
3. PostgreSQL needs `pg_dump` on `PATH`, preferably the **same major version** as
   the scratch server. An older `pg_dump` cannot dump a newer server. MySQL needs
   a reachable **MySQL 8.0.16+** server (enforced `CHECK` constraints); DDL is read
   with SQLx `SHOW CREATE TABLE`, so neither `mysql` nor `mysqldump` is needed by
   the generator itself. MySQL 8.4 is the recommended reproducible reference.
4. Supply `--scratch-db-url` or **only** `NYRO_SCHEMA_DATABASE_URL`. There is no
   fallback to application database settings (`DATABASE_URL`, `NYRO_DATABASE_URL`,
   etc.). The flag overrides the dedicated environment variable. Use a TCP URL
   with explicit host, user, and database; database names support ASCII letters,
   digits, `_`, and `-`. System/default databases are rejected. Port defaults are
   pinned to 5432/3306 rather than inherited from `PGPORT`. Only TLS query
   parameters are accepted (`sslmode`, `sslrootcert`, `sslcert`, `sslkey` for
   PostgreSQL; `ssl-mode`, `ssl-ca`, `ssl-cert`, `ssl-key` for MySQL); connection
   overrides such as `dbname`, `host`, `options`, or `socket` are rejected.
   URL-encode passwords. Prefer the dedicated environment variable over putting
   credentials in shell history or process arguments. Local temporary trust auth
   or a temporary password is appropriate; do not supply production credentials.

Example, **after creating the named empty databases on disposable local servers**
(the ports below are examples, not discovery/default application endpoints):

```bash
cargo build -p nyro-tools

NYRO_SCHEMA_DATABASE_URL='postgresql://schema_user@127.0.0.1:25439/nyro_schema_pg' \
  target/debug/nyro-tools dump-schema --backend postgres \
  --output deploy/schema/postgres.sql

NYRO_SCHEMA_DATABASE_URL='mysql://schema_user@127.0.0.1:23306/nyro_schema_mysql' \
  target/debug/nyro-tools dump-schema --backend mysql \
  --output deploy/schema/mysql.sql
```

The tool rejects existing objects **before bootstrap**, including empty tables.
It never clears, drops, or creates databases. Successful migration leaves the
scratch database populated with schema; another run must use another new empty
database. A failed migration may also leave partial DDL in the disposable
scratch database; inspect it if needed, then dispose of the temporary instance
rather than retrying against it. Database error codes/operation names are
reported on stderr, but raw driver/dump errors and credentials are suppressed.

Use **`--output`**, not `> deploy/schema/...`: the tool first captures and
validates all SQL, then writes a temporary file beside the destination, flushes
it, and atomically replaces the artifact. Failures before replacement leave the
previous artifact untouched. Without `--output`, stdout contains only a complete
successful SQL dump (no logs); shell redirection into a tracked file would still
truncate that file *before* the tool runs and is therefore unsafe.

### Fidelity, determinism, and validation

- PostgreSQL uses `pg_dump --schema-only --no-owner --no-privileges`. It preserves
  constraints, indexes, types, collations, defaults, and schema-qualified names.
  Volatile server/client version and timestamp banner lines are omitted, random
  `psql` restrict/unrestrict tokens are normalized (guards retained), and the
  version-specific `transaction_timeout` session setting is omitted. No migration
  DDL is reconstructed or renamed by the tool.
- MySQL uses full `SHOW CREATE TABLE` output, including indexes, PKs, FKs, CHECKs,
  binary columns/collations, and engine/table options. Tables are emitted in a
  deterministic parent-before-child FK order with lexical tie-breaking and safe
  identifier quoting. Cyclic or cross-database dependencies and non-table objects
  are rejected rather than silently dropping constraints or disabling FK checks.
- Both backends verify final tables `models`, `model_backends`,
  `api_key_models`, and `model_rating_prefixes` and reject leftover `routes`, `route_targets`, or
  `api_key_routes`. Generated constraint/index names can still retain a legacy
  prefix after table renames; these are the real database names and are not
  cosmetically rewritten.
- Determinism is for the same migrations and server/tool versions/configuration.
  Pin those versions for byte-for-byte comparison; semantic formatting differences
  across PostgreSQL or MySQL versions are not universally normalized. Generate
  twice on independent fresh scratch databases and compare the files. Restore
  reference SQL only into another disposable empty database when testing it.

These are review/reference artifacts, not an assurance that importing DDL and
then restarting any historical bootstrap version is safe. Older MySQL bootstrap
versions used unconditional `CREATE INDEX` statements, which failed on repeated
bootstrap. Current storage guards index creation; schema generation still runs
bootstrap only once on an empty database and is not a migration redesign.

Focused tests: `cargo test -p nyro-tools schema::tests`, followed by
`cargo check -p nyro-tools`. The two ignored nonempty-rejection tests require
separate **new empty disposable databases** and leave one empty sentinel table:

```bash
NYRO_SCHEMA_TEST_POSTGRES_URL='<new-empty-postgres-scratch-url>' \
NYRO_SCHEMA_TEST_MYSQL_URL='<new-empty-mysql-scratch-url>' \
  cargo test -p nyro-tools schema::tests:: -- --ignored
```

Missing test URLs fail explicitly; tests never fall back to production/app URLs.

For storage conformance against the regenerated artifacts, provision **another new
empty database per backend**, with a name starting `nyro_test_` or ending `_test`:

```bash
NYRO_TEST_RATINGS_DATABASES_ONLY=1 \
NYRO_TEST_RATINGS_PRECREATE_REFERENCE=1 \
NYRO_TEST_POSTGRES_RATINGS_URL='<new-empty-postgres-reference-test-url>' \
NYRO_TEST_MYSQL_RATINGS_URL='<new-empty-mysql-reference-test-url>' \
  cargo test -p nyro-core --test storage_model_ratings
```

The precreate flag makes the test load the generated SQL itself before exercising
migration/storage operations. Do **not** manually load those same databases first.
To test databases into which you already restored the artifacts separately, omit
`NYRO_TEST_RATINGS_PRECREATE_REFERENCE`. The tests may drop/recreate the ratings
table and leave test data; do not share their databases with generation or other
running tests. Missing external URLs skip that backend rather than verify it.
The effort/performance conformance suite has separate opt-in variables and also
simulates legacy-table migration; give it **its own** disposable test databases:

```bash
NYRO_TEST_PERFORMANCE_DATABASES_ONLY=1 \
NYRO_TEST_POSTGRES_PERFORMANCE_URL='<dedicated-postgres-performance-test-url>' \
NYRO_TEST_MYSQL_PERFORMANCE_URL='<dedicated-mysql-performance-test-url>' \
  cargo test -p nyro-core --test storage_effort_performance
```

Outcome/correlation conformance has its own opt-in and may mutate schema/data to
exercise legacy migration and write rollback. Provision **separate new empty**
databases named `nyro_outcomes_test` on disposable servers (not the databases used
for reference generation or other suites):

```bash
NYRO_TEST_OUTCOME_DATABASES_ONLY=1 \
NYRO_TEST_POSTGRES_OUTCOMES_URL='<new-empty-postgres-outcomes-test-url>' \
NYRO_TEST_MYSQL_OUTCOMES_URL='<new-empty-mysql-outcomes-test-url>' \
  cargo test -p nyro-core --test log_outcomes_storage
```

Keep missing external URLs distinct from verified backend coverage; optional
external cases do not prove conformance unless explicitly configured and run.
Retain temporary services until all generation, restore, and conformance runs have
finished, then stop only those explicitly provisioned disposable services.

---

## 迁移说明

Nyro 采用尽量幂等的增量迁移策略（不代表整个 bootstrap 支持重复执行；参见上面的 MySQL 注意事项）：`INIT_SQL` 创建旧名称表（如 `routes`、`route_targets`、`api_key_routes`），`migrate()` 末尾执行 rename：

```
routes             → models
route_targets      → model_backends
api_key_routes     → api_key_models
routes.strategy    → models.balance
routes.virtual_model → models.name（合并至 name 列）
request_logs.route_id   → request_logs.model_id
request_logs.route_name → request_logs.model_name
settings.key       → settings.name
api_keys.key       → api_keys.token
```

旧版 `providers.protocol_endpoints` JSON 在迁移时会转换为
`provider_protocol_endpoints` 行；包含多个协议声明的 Provider 会迁移为
`adaptive`，并保留原有默认协议、Base URL 和共享 API Key。

所有 rename 操作均为幂等：先检查旧列存在且新列不存在，才执行 `ALTER TABLE RENAME`。
