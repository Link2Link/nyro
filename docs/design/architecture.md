# Nyro AI Gateway — 架构设计

---

## 1. 产品定位与部署形态

Nyro 是一个 **AI 协议网关（AI Gateway）**：在 AI 客户端工具与模型提供商之间做实时协议转换与统一调度。任意使用 OpenAI / Anthropic / Gemini SDK 的客户端无需改代码，仅修改 `base_url` 即可路由到任意 LLM Provider。既可作为**桌面应用**本地零部署运行，也可作为**独立服务端**自托管或团队共享，管理与配置保持私有可控。

```
Claude Code · Codex CLI · Gemini CLI · OpenCode
     OpenAI SDK · Anthropic SDK · Gemini SDK
              Any HTTP API Client
                      ↓
              Nyro AI Gateway
            (localhost:19530)
                      ↓
    OpenAI · Anthropic · Google · DeepSeek
    MiniMax · xAI · GLM · Ollama · ...
```

**部署形态：**

| 形态 | 实现 | 适用场景 |
|---|---|---|
| Desktop | Tauri v2 桌面应用（macOS / Windows / Linux） | 个人开发者，零部署，数据不离开本机 |
| Server `--mode all` | 独立 Rust 二进制，Proxy + Admin API + 内嵌 WebUI | 自托管、团队共享（默认） |
| Server `--mode proxy` | 同上，仅启动代理端口 `:19530` | 分布式纯调度节点，无管理面 |
| Server `--mode admin` | 同上，仅启动管理端口 `:19531` | 内部系统对接控制面；可搭配 `--no-default-features`（slim）去除内嵌 WebUI |
| Server Standalone | `--config config.yaml`，MemoryStorage，无 Admin | 边缘/最小化部署，YAML 静态配置 |

核心原则：`nyro-core` 只暴露纯 Rust API（struct + async fn），**不感知传输层**。Desktop 版通过 Tauri IPC 调用，Server 版通过 HTTP REST 调用。

---

## 2. Workspace 分层

```
nyro/
├── Cargo.toml                   # Rust workspace
├── crates/
│   └── nyro-core/
│       └── src/
│           ├── lib.rs            # 14 个顶层 pub mod + Gateway / GatewayConfig 根类型
│           ├── proxy/            # 代理面
│           │   ├── mod.rs
│           │   ├── auth.rs
│           │   ├── client.rs     # ProxyClient（HTTP 调用封装）
│           │   ├── context.rs    # RequestContext / ContextBag
│           │   ├── handler.rs    # models_list 只读端点（≤110 行）
│           │   ├── intake.rs     # 请求接入预处理
│           │   ├── observability.rs  # 日志工具（header 脱敏、URL 脱敏等）
│           │   ├── security.rs   # 安全过滤
│           │   ├── server.rs     # axum HTTP Server 启动
│           │   ├── stream.rs     # StreamBridge 状态机
│           │   ├── dispatcher/   # 单点编排管线
│           │   │   ├── mod.rs        # dispatch_pipeline / dispatch / error_response
│           │   │   ├── accumulator.rs
│           │   │   ├── auth.rs       # authorize_model_access / get_provider
│           │   │   ├── buffered.rs   # Native/Raw-Wire 成功响应的共享语义生命周期
│           │   │   ├── non_stream.rs # handle_non_stream / handle_non_stream_via_upstream_stream
│           │   │   ├── stream.rs     # handle_stream
│           │   │   └── util.rs
│           │   ├── planner/      # 协议协商
│           │   │   ├── mod.rs        # ProtocolPlan / ProtocolMode 等 re-export
│           │   │   └── negotiator.rs # negotiate() / RoutingStrategy / OrderedStrategy
│           │   └── ingress/      # 5 个薄 ingress shell（按协议族分目录）
│           │       ├── mod.rs
│           │       ├── openai_compatible/
│           │       │   ├── mod.rs
│           │       │   ├── chat_completions.rs   # decode → dispatch_pipeline
│           │       │   └── embeddings.rs
│           │       ├── openai_responses/
│           │       │   ├── mod.rs
│           │       │   └── responses.rs
│           │       ├── anthropic_messages/
│           │       │   ├── mod.rs
│           │       │   └── messages.rs
│           │       └── google_generative/
│           │           ├── mod.rs
│           │           └── generate_content.rs
│           ├── plugin/           # 扩展框架（PluginKernel + 生命周期插点）
│           │   ├── mod.rs        # PluginKernel / CapabilityKind / PluginManifest
│           │   └── phase.rs      # Phase / PhaseHook / PhaseCtx / PhaseOutcome /
│           │                     # ResponseView / HostContext / ResponseStats /
│           │                     # PhaseHookRegistration / PhaseHookRegistry
│           ├── protocol/         # 协议转换引擎
│           │   ├── mod.rs        # ProviderProtocols / ResolvedEgress 等
│           │   ├── ids.rs        # ProtocolEndpoint（ProtocolId 为别名）/ ProtocolCapabilities
│           │   ├── traits.rs     # EndpointHandler trait + 6 个 codec trait
│           │   ├── registry.rs   # ProtocolRegistry / EndpointRegistration
│           │   ├── ir/           # 统一内部表示（IR）
│           │   │   ├── mod.rs
│           │   │   ├── request.rs   # AiRequest
│           │   │   ├── response.rs  # AiResponse
│           │   │   ├── stream.rs    # AiStreamDelta
│           │   │   ├── usage.rs     # Usage
│           │   │   └── ...          # envelope / ext / vendor_ext / cache / error 等
│           │   └── codec/        # 编解码器 + EndpointHandler 注册壳
│           │       ├── mod.rs
│           │       ├── reasoning.rs       # think-tag 提取工具
│           │       ├── tool_correlation.rs
│           │       ├── openai/
│           │       │   ├── compatible/    # chat_completions + embeddings
│           │       │   └── responses/     # OpenAI Responses API
│           │       ├── anthropic/
│           │       │   └── messages/
│           │       └── google/
│           │           └── gemini/
│           ├── conversion/       # 每 target 的统一协议转换规划
│           │   ├── plan.rs       # PassThrough / Native IR / Raw-Wire Compat
│           │   ├── resolver.rs   # 选路、Compat profile/session 与 ResolvedConversion
│           │   ├── prepared.rs   # PreparedBody / PreparedSession / 三策略准备
│           │   ├── outcome.rs    # ConversionAttempt / retry / health
│           │   └── wire_patch.rs # IR mutation → 原始 wire body patch
│           ├── provider/         # 厂商扩展层
│           │   ├── mod.rs
│           │   ├── vendor.rs     # Vendor trait / ProviderCtx / VendorRegistration
│           │   ├── vendor_ext.rs # VendorExtension trait / VendorCtx / ExtensionRegistration
│           │   ├── registry.rs   # VendorRegistry
│           │   ├── metadata.rs   # VendorMetadata / Label / AuthMode
│           │   ├── outbound.rs   # OutboundRequest
│           │   ├── inbound.rs    # InboundResponse
│           │   ├── common/
│           │   │   ├── openai.rs     # OpenAI 兼容共用逻辑
│           │   │   └── pipeline.rs   # 7 步 build_request / parse_response 自由函数
│           │   ├── openai/           # OpenAiVendor + OpenAIFamilyExt
│           │   │   └── codex/        # OpenAiCodexChannel（OAuth channel）
│           │   ├── anthropic/        # AnthropicVendor + AnthropicFamilyExt
│           │   │   └── claude_code/  # AnthropicClaudeCodeChannel
│           │   ├── google/           # GoogleVendor + GoogleFamilyExt
│           │   ├── vertexai/         # VertexVendor
│           │   ├── deepseek/ · moonshotai/ · zhipuai/ · minimax/
│           │   ├── xai/ · zai/ · nvidia/ · openrouter/ · ollama/ · custom/
│           │   └── ...
│           ├── admin/            # AdminService 管理面（按职责拆分）
│           │   ├── mod.rs
│           │   ├── extensions.rs # list_loaded_extensions（PluginKernel 聚合）
│           │   ├── providers.rs · oauth.rs · routes.rs · api_keys.rs
│           │   ├── settings.rs · observability.rs · import_export.rs
│           │   ├── model_catalog.rs · auth_data.rs · route_data.rs
│           │   └── session_tests.rs
│           ├── error.rs          # GatewayError taxonomy
│           ├── router/           # TargetSelector / HealthRegistry
│           ├── storage/          # 多后端存储（sqlite / postgres / mysql / memory）
│           ├── db/               # SQLite schema / migrate()
│           ├── logging/          # LogEntry / send_log
│           ├── cache/
│           ├── integrations/     # HookRegistry（旧版 request/response hook）
│           └── auth/
├── src-tauri/
├── src-server/
└── webui/
```

**依赖关系：**

```mermaid
graph TD
    nyroCoreLib["nyro-core (lib)"]
    srcTauri["src-tauri (Desktop binary)"]
    srcServer["src-server (Server binary)"]
    webui["webui (React + TypeScript)"]
    tauriIPC["Tauri IPC"]
    httpREST["HTTP REST :19531"]

    srcTauri --> nyroCoreLib
    srcServer --> nyroCoreLib
    webui --> tauriIPC
    webui --> httpREST
    tauriIPC --> srcTauri
    httpREST --> srcServer
```

**nyro-core 顶层 `pub mod`（lib.rs，共 14 个）：**

```
admin · auth · config · conversion · db · error · integrations · logging
plugin · protocol · provider · proxy · router · storage
```

**核心 API：**

```
Gateway::new(config)      → 初始化数据库、启动代理服务
Gateway::start_proxy()    → 启动 axum HTTP Server（代理面）
Gateway::admin()          → 返回 AdminService，提供全部管理操作
  ├── .list_models()
  ├── .create_model(input)
  ├── .list_providers()
  ├── .create_provider(input)
  ├── .test_provider(id)
  ├── .list_api_keys()
  ├── .query_logs(filter)
  ├── .get_stats_overview()
  ├── .list_loaded_extensions()  ← PluginKernel 聚合 manifest
  └── ...
Gateway::shutdown()       → 优雅关闭
```

`AdminService` 是管理面唯一入口；`admin/` 子模块按功能职责分布，不引入新传输层抽象。

---

## 3. 协议转换架构

### 3.0 cc-switch 兼容层（raw-wire parity）

跨协议转换的线级行为由 `crates/nyro-ccswitch-compat` 提供：该 crate 从 cc-switch（commit `eb69e492`，MIT）机械移植了全部转换核心（`transform_*`、`streaming_*`、SSE 工具、内容解码等，含内嵌测试），并封装为字节进字节出的 `CompatEngine`。`nyro-core` 的 dispatcher（`proxy/dispatcher/compat.rs`）在以下方向选用该引擎，绕过 IR 往返以获得与 cc-switch 完全一致的线级行为：

| ingress → egress | 说明 |
|---|---|
| Anthropic Messages → OpenAI Chat / Responses / Gemini | Claude Code 客户端接 OpenAI/Gemini 系上游 |
| OpenAI Responses → OpenAI Chat / Anthropic Messages | Codex 客户端接 Chat 或 Claude 系上游 |
| OpenAI Responses → Responses（xai） | xAI 原生 Responses 的 namespace 扁平化/还原与 xAI sanitize |
| Anthropic Messages → Anthropic Messages（DeepSeek/MiMo 系） | 同协议直通 + cc-switch 的 thinking 历史回放归一化与 DeepSeek 官方 effort 剥离 |

对齐程度由 `scripts/check_cc_switch_parity_inventory.py` 审计 `tests/parity_inventory.toml`（1168 个源测试：511 直接移植 + 149 断言级映射 + 508 经批准排除），要求 `--require-complete` 通过。完整的移植分析、模块映射与维护指南见 [cc-switch-porting-report.md](./cc-switch-porting-report.md)。

### 3.1 核心设计原则

- **统一错误 taxonomy**：`GatewayError` 覆盖 15 种错误类型，每个错误有稳定 code、HTTP status、user message、internal detail 和 retryable 标志。
- **请求生命周期追踪**：`RequestContext` 携带 request_id、deadline、cancellation token、outcome，以及类型键扩展袋 `ContextBag`，端到端贯穿所有层（见 §4）。
- **确定性协议协商**：`negotiate()`（`proxy/planner/negotiator.rs`）实现三级 egress 解析（Exact → Same-family → Provider Default），`ProtocolRegistry` 统一别名规范化。
- **Pass-Through 已落地**：`ingress == egress` 且对应 leg 无 mutation 时，dispatcher 绕过跨协议 IR 往返；请求侧保留解析后的原生 JSON 并应用窄范围默认值/安全归一化，响应侧在安全时直接转发 body / SSE 字节。
- **完整字段映射**：每个 codec 明确处理已知字段；vendor-specific 字段走三段化路径，不隐式丢弃。
- **Vendor 单点接口**：`dispatch_pipeline` 只通过 `Vendor` trait 与厂商层交互，一次注册覆盖请求/响应编解码与流式处理的完整生命周期。
- **五阶段生命周期**：请求/响应全程经过 OnRequest / OnAccess / OnUpstream / OnResponse / OnLog 五个插点，通过 `PhaseHook` 做非侵入扩展（见 §4）。

### 3.2 完整调用流程

```
+--------------------+                  +------------------------------------------+
| Client / CLI / SDK | -- HTTP/SSE --> | Ingress Shell（proxy/ingress/<family>/）   |
|                    |                  |  RequestContext 注入（axum Extension）      |
|                    |                  |  decode body → AiRequest（IR）             |
+--------------------+                  +-------------------+----------------------+
                                                            |
                                                            ▼
                                         +------------------------------------------+
                                         | dispatch_pipeline（薄包装）               |
                                         |  HostContext::new(&gw)                    |
                                         |  ↓ dispatch_pipeline_inner(ctx, req, host)|
                                         +-------------------+----------------------+
                                                            |
                                  ┌─ Phase ①  OnRequest ───┤  路由键派生前
                                  │  PhaseHook chain        │  可改写 request.model
                                  │  ShortCircuit → 直接返回 │  Reject → 渲染错误
                                  └─────────────────────────┤
                                                            │
                                              Route lookup（model_cache）
                                              Auth / Quota（authorize_model_access）
                                                            │
                                  ┌─ Phase ②  OnAccess ────┤  身份 + 路由已定
                                  │  PhaseHook chain        │  可 Reject（限流/鉴权策略）
                                  └─────────────────────────┤
                                                            │
                                         Target iteration（HealthRegistry 感知）
                                                            │
                                  ┌─────────── 每个 target ──────────────────────┐
                                  │  negotiate() → egress / base_url              │
                                  │  VendorRegistry.get_vendor(vendor_id)         │
                                  │                                               │
                                  │  ┌─ Phase ③  OnUpstream ──────────────────┐  │
                                  │  │  per-attempt（重试循环内）               │  │
                                  │  │  可 ShortCircuit（缓存命中）             │  │
                                  │  └────────────────────────────────────────┘  │
                                  │                                               │
                                  │  provider_ctx = ProviderCtx{...}              │
                                  │  build outbound（passthrough_run | 7 步）      │
                                  │  CallCtx{ req_ext = ctx.extensions.clone() }  │
                                  │                                               │
                                  │  ┌────────────── handlers ──────────────────┐ │
                                  │  │ is_stream                                 │ │
                                  │  │  → handle_stream                          │ │
                                  │  │    · IR path: spawn 内逐 AiStreamDelta    │ │
                                  │  │        ┌─ Phase ④ OnResponse(Stream) ─┐  │ │
                                  │  │    · SSE passthrough: 不接 OnResponse   │ │
                                  │  │ force_upstream_stream                    │ │
                                  │  │  → handle_non_stream_via_upstream_stream  │ │
                                  │  │        ┌─ Phase ④ OnResponse(Full) ──┐  │ │
                                  │  │ else                                     │ │
                                  │  │  → handle_non_stream                     │ │
                                  │  │        ┌─ Phase ④ OnResponse(Full) ──┐  │ │
                                  │  │    · LogBuilder.emit()                   │ │
                                  │  │        └▶ 注入 ResponseStats → ctx.ext  │ │
                                  │  └──────────────────────────────────────────┘ │
                                  │  status<400 → record_success, return          │
                                  │  retryable → continue; else → return          │
                                  └───────────────────────────────────────────────┘
                                                            │
                                         ┌─ Phase ⑤  OnLog ┤  单一汇聚点（所有返回路径）
                                         │  fire-and-forget │  只读 ctx.extensions 的
                                         │  不可改/不可短路  │  ResponseStats 快照
                                         └──────────────────┤
                                                            │
                                              Return to Client（JSON / SSE）
```

### 3.3 内部表示（IR）

位于 `crates/nyro-core/src/protocol/ir/`，定义统一内部结构：

- `AiRequest`（`ir/request.rs`）：入站请求，含消息列表、工具定义、模型参数
- `AiResponse`（`ir/response.rs`）：出站响应，含 content / tool_calls / usage / reasoning_content
- `AiStreamDelta`（`ir/stream.rs`）：流式增量事件，支持 reasoning delta、text、tool_call
- `Usage`（`ir/usage.rs`）：prompt_tokens / completion_tokens / total_tokens / cache_read_tokens

**vendor-specific 字段命名约定（存于 IR extra 字段）：**

| 前缀 | 用途 |
|---|---|
| `__anthropic_raw_*` | Anthropic cache_control / exotic blocks 无损往返 |
| `__google_raw_*` | Google systemInstruction / built-in tools / generationConfig |
| `__emb_*` | Embeddings 已知字段（input / dimensions / encoding_format / user） |
| `__vendor_ingress` | 未知 vendor 字段集合（由 VendorFieldPolicy 决定是否转发） |

---

## 4. 请求生命周期与扩展框架

> 权威设计文档：[docs/design/lifecycle.md](lifecycle.md)。本节提供概要，细节以 RFC 为准。

### 4.1 五阶段定义

| 阶段 | 时机 | 可做的事 | PhaseOutcome |
|---|---|---|---|
| **OnRequest** | 路由键派生前 | 改写 `request.model`、添加头 | Continue / ShortCircuit / Reject |
| **OnAccess** | 鉴权完成后 | 限流拒绝、自定义 ACL | Continue / ShortCircuit / Reject |
| **OnUpstream** | 上游调用前，per-attempt | 缓存命中短路、参数注入 | Continue / ShortCircuit / Reject |
| **OnResponse** | 响应解码后（非流式: Full；流式: 逐 delta） | 整形输出、屏蔽字段、写缓存 | Continue / ShortCircuit / Reject（非流式）；仅 Continue（流式） |
| **OnLog** | 管线边界（所有返回路径汇聚后） | 只读采样、指标上报、外部投递 | Continue（终态，不可短路） |

### 4.2 核心类型

```rust
// 钩子接口（inventory::submit! 注册，进程内静态链接）
#[async_trait]
pub trait PhaseHook: Send + Sync {
    fn name(&self) -> &'static str;
    fn phase(&self) -> Phase;
    async fn run(&self, ctx: &mut PhaseCtx<'_>) -> PhaseOutcome;
}

// 每次调用时传给 hook 的上下文四件套
pub struct PhaseCtx<'a> {
    pub req_ctx:  &'a mut RequestContext,  // 端到端请求上下文（含 ContextBag）
    pub request:  &'a mut AiRequest,       // 协议中性 IR
    pub response: ResponseView<'a>,        // Pending / Full / Stream
    pub host:     &'a HostContext<'a>,     // 网关能力边界（存储/配置/HTTP 等）
}

// ResponseStats: emit() 单点注入，OnLog 及 OnLogHook 读取
pub struct ResponseStats {
    pub client_status:       u16,
    pub upstream_status:     Option<u16>,
    pub usage:               Usage,
    pub upstream_latency_ms: Option<i64>,
    pub ttfb_ms:             Option<i64>,
    pub stream_chunks:       u32,
}
```

### 4.3 数据通道

```
RequestContext.extensions: ContextBag
  └─ 类型键（TypeId）→ Box<dyn Any + Send + Sync>
     共享引用（Arc<Mutex<HashMap>>），clone 后仍指向同一份数据

写入：LogBuilder::emit() → ctx.extensions.insert::<ResponseStats>(...)
读取：OnLog / OnLogHook → ctx.extensions.get::<ResponseStats>()
```

### 4.4 PluginKernel 与 admin 视图

`PluginKernel`（`plugin/mod.rs`）聚合所有 `inventory` 注册表：

```
PluginKernel::global()
  ├── HookRegistry（旧版 integrations）
  ├── VendorRegistry
  ├── ProtocolRegistry
  └── PhaseHookRegistry
        → manifests() → [{id, capability}]
              ↓
        GET /api/v1/system/extensions
        Tauri: get_loaded_extensions
        WebUI: /extensions 只读页
```

### 4.5 框架级 vs 插件级（当前状态）

```
框架级 ✅ 已交付
  五个 PhaseHook 插点接线（OnRequest/OnAccess/OnUpstream/OnResponse/OnLog）
  RequestContext.extensions（ContextBag）端到端贯穿
  ResponseStats 单点注入（emit()）
  HostContext 稳定边界
  PhaseHookRegistry（inventory 注册，空注册表零开销 no-op）
  PluginKernel + admin "已加载扩展" 只读视图

插件级 🔲 待做（A4）
  限流插件（OnAccess → Reject）
  语义缓存插件（OnUpstream ShortCircuit + OnResponse 落缓存）
  可观测性 exporter（OnLog 消费 ResponseStats → OTel / Prometheus）
```

---

## 5. 协议层（codec/）详情

### 5.1 EndpointHandler 注册体系

每个 dialect 的注册壳位于 `codec/<family>/<dialect>/` 对应目录，通过 `inventory::submit!` 自动注册进 `ProtocolRegistry`：

```rust
inventory::submit! {
    EndpointRegistration { make: || Box::new(XxxHandler) }
}
// ProtocolRegistration 为 EndpointRegistration 的向后兼容别名
```

| 目录 | 注册的 ProtocolId（ProtocolEndpoint） |
|---|---|
| `codec/openai/compatible/` | `openai/chat/v1`、`openai/embeddings/v1` |
| `codec/openai/responses/` | `openai/responses/v1` |
| `codec/anthropic/messages/` | `anthropic/messages/2023-06-01` |
| `codec/google/gemini/` | `google/generate/v1beta` |

`ProtocolId` 现为 `ProtocolEndpoint`（`protocol/ids.rs`）的类型别名，保持向后兼容。

### 5.2 EndpointHandler trait 与 codec trait

`EndpointHandler`（原 `ProtocolHandler`，`protocol/traits.rs`）：

```rust
trait EndpointHandler: Send + Sync {
    fn id(&self) -> ProtocolEndpoint;
    fn capabilities(&self) -> ProtocolCapabilities;
    fn make_request_decoder(&self)         -> Box<dyn RequestDecoder>;
    fn make_request_encoder(&self)         -> Box<dyn RequestEncoder>;
    fn make_response_decoder(&self)        -> Box<dyn ResponseDecoder>;
    fn make_response_encoder(&self)        -> Box<dyn ResponseEncoder>;
    fn make_stream_response_decoder(&self) -> Box<dyn StreamResponseDecoder>;
    fn make_stream_response_encoder(&self) -> Box<dyn StreamResponseEncoder>;
}
```

6 个 codec trait（`protocol/mod.rs`）：`RequestDecoder`、`RequestEncoder`、`ResponseDecoder`、`ResponseEncoder`、`StreamResponseDecoder`、`StreamResponseEncoder`。流式解析在 protocol 层（`StreamResponseDecoder::parse_chunk` / `finish`），**非** Vendor 层。

### 5.3 ProtocolCapabilities 矩阵

| 字段 | 类型 | 含义 |
|---|---|---|
| `streaming` | bool | 支持 SSE 流式 |
| `tools` / `function_calling` | bool | 支持 tool call |
| `reasoning` / `extended_reasoning` | bool | 支持 thinking / reasoning |
| `embeddings` | bool | Embeddings 端点 |
| `force_upstream_stream` | bool | 强制上游 streaming（Responses API） |
| `override_model_in_body` | bool | model 写入 URL path（Google） |
| `unknown_field_policy` | VendorFieldPolicy | Pass / Drop |
| `lossy_default_reject` | bool | lossy 转换默认拒绝 |

### 5.4 Codec 完整字段映射

**OpenAI Chat**：完整映射 logprobs、seed、response_format、parallel_tool_calls、audio 等 20+ 字段；reasoning 字段透传。

**OpenAI Responses**：`force_upstream_stream=true`；独立 decoder/encoder/parser/formatter；reasoning_item 的 summary text 提取。

**Anthropic Messages**：cache_control、thinking config、context_management、exotic blocks（Document / InputAudio）保留 `__anthropic_raw_*` 做无损往返；built-in tools（web_search_call）作为 sentinel ToolDef 处理。

**Google GenerateContent**：完整 generationConfig（20+ fields）、safety_settings、built-in tools（googleSearch / codeExecution）；`__google_generation_config` 在 encoder 中被 model 参数 overlay。

**OpenAI Embeddings**：`VendorFieldPolicy::Drop`；`__emb_*` 明确解析；unknown fields 进 `__vendor_ingress` 但不转发。

### 5.5 语义工具（codec/reasoning.rs & codec/tool_correlation.rs）

**reasoning.rs**：
- `normalize_response_reasoning`：结构化字段优先，`<think>` tag 兜底提取
- `split_think_tags`：多 `<think>` block 支持，未闭合 tag 保留为文本

**tool_correlation.rs**：`normalize_request_tool_results`，统一 tool_call_id 关联（精确 ID → content hint → 工具名 hint → FIFO fallback → 自动补合成 assistant message）。

---

## 6. 厂商扩展层（provider/）

三层职责分离：

```
protocol/codec/   ← 序列化层：AiRequest/AiResponse ↔ wire-format JSON
provider/         ← 编排层：Vendor trait（build_request / parse_response）+ VendorExtension hooks
```

### 6.1 Vendor trait（原 ProviderAdapter）

`dispatcher` 的唯一接触点（`provider/vendor.rs`）：

```rust
#[async_trait]
pub trait Vendor: Send + Sync + 'static {
    // 标识 / 元数据
    fn scope(&self) -> VendorScope;              // Vendor | Channel
    fn vendor_id(&self) -> &'static str;
    fn supported_protocols(&self) -> &'static [ProtocolId];
    fn metadata(&self) -> &'static VendorMetadata;

    // Auth / URL
    fn auth_headers(&self, ctx: &VendorCtx) -> HeaderMap;
    fn build_url(&self, ctx: &VendorCtx, base_url: &str, path: &str) -> String;

    // 编解码 hook（可选，默认 no-op）
    async fn pre_request(&self, ctx, req: &mut AiRequest, gw: &Gateway);
    async fn pre_encode(&self, ctx, req: &mut AiRequest);
    async fn post_encode(&self, ctx, body: &mut Value, headers: &mut HeaderMap);
    async fn pre_parse(&self, ctx, body: &mut Value);
    async fn post_parse(&self, ctx, resp: &mut AiResponse);

    // 流式 hook
    async fn on_stream_raw_chunk(&self, ctx, chunk: &str);
    async fn on_stream_delta(&self, ctx, delta: &mut AiStreamDelta);

    // 编排（required）
    async fn build_request(&self, req: &mut AiRequest, ctx: &ProviderCtx)
        -> Result<OutboundRequest, GatewayError>;
    async fn parse_response(&self, resp: InboundResponse, ctx: &ProviderCtx)
        -> Result<AiResponse, GatewayError>;
    fn map_error(&self, status: u16, body: Value) -> GatewayError;
    fn validate_environment(&self, provider: &Provider) -> Result<(), GatewayError>;

    // PassThrough 声明（no mutation → dispatcher 走 passthrough 路径）
    fn declared_request_mutations(&self) -> bool { false }
    fn declared_response_mutations(&self) -> bool { false }
}
```

**7 步 build_request pipeline**（`provider/common/pipeline.rs`）：
`pre_request` → `normalize_tool_results` → `pre_encode` → `codec_encode` → `post_encode` → `auth_headers` → `build_url`

**ProviderCtx**（`provider/vendor.rs`）：

```rust
pub struct ProviderCtx<'a> {
    pub provider:             &'a Provider,
    pub protocol:             ProtocolId,        // 即 ProtocolEndpoint
    pub egress_base_url:      &'a str,
    pub api_key:              &'a str,
    pub actual_model:         &'a str,
    pub credential:           Option<&'a StoredCredential>,
    pub gw:                   &'a Gateway,
    pub disable_default_auth: bool,
}
```

### 6.2 VendorExtension（channel / family ext）

`VendorExtension`（`provider/vendor_ext.rs`）仍存在，包含 9 个 hook（auth_headers / build_url / pre_encode / post_encode / pre_parse / post_parse / on_stream_raw_chunk / on_stream_delta / pre_request）。

**关系：**
- `Vendor` 通过 blanket `impl<T: Vendor> VendorExtension for T` 自动实现 `VendorExtension`
- Channel-only 类型（`OpenAiCodexChannel`、`AnthropicClaudeCodeChannel`）仅 impl `VendorExtension`

**两套注册（均通过 `inventory::submit!`）：**

```rust
// 完整 vendor
inventory::submit! { VendorRegistration { make: || Box::new(XxxVendor) } }
// Channel / family ext
inventory::submit! { ExtensionRegistration { make: || Box::new(XxxChannel) } }
```

`VendorRegistry::resolve(provider, protocol_id)` 返回 `Arc<dyn VendorExtension>`，内部通过 `VendorAsExt` 包装统一两类注册。

### 6.3 共用 helpers（provider/common/openai.rs）

所有 OpenAI 兼容厂商共用：`openai_bearer_auth_headers`、`openai_build_url`、`openai_map_error`、`openai_compat_build_request`、`openai_compat_parse_response`、`GenericOpenAICompatibleAdapter`、`ThinkTagExtractingParser`。

### 6.4 厂商列表

| 厂商 | vendor_id | 特殊处理 |
|---|---|---|
| OpenAI | `openai` | 含 `codex` channel（OAuth） |
| Anthropic | `anthropic` | `x-api-key` + `anthropic-version`；含 `claude-code` channel |
| Google | `google` | default channel：URL 追加 `?key=<api_key>`；含 `antigravity` channel（Google AI Pro 订阅 OAuth，见 6.5）与 `gemini-cli` channel（Gemini CLI / Code Assist 订阅 OAuth，见 6.5） |
| Vertex AI | `vertexai` | Service account auth + 区域 endpoint |
| DeepSeek / Moonshot / GLM (zhipuai) / MiniMax / xAI / ZAI / OpenRouter / Nvidia / Ollama | 各自 vendor_id | 委托 `GenericOpenAICompatibleAdapter` / openai_compat_* |
| OpenCode Go | `opencode-go` | 三端点自适应（chat/responses/messages，共享 Key）+ 按模型的硬编码端点路由；每个请求必须携带会话标识 `x-opencode-session`（见 6.6） |
| custom | `custom` | 用户自定义 vendor preset |

### 6.5 Google 订阅通道（Antigravity / Gemini CLI OAuth）

Google 订阅共两条 channel，均经 Code Assist 内部 API（`cloudcode-pa.googleapis.com/v1internal`）使用订阅配额，共用一套 driver 实现（`auth/drivers/google.rs::GoogleSubscriptionDriver`，按 channel 选择 OAuth 客户端 / client secret / UA / onboarding host）：

- **`google/antigravity`**（driver key `google`）：Google AI Pro / Ultra 订阅，Antigravity IDE 公共 OAuth 客户端（对齐 CLIProxyAPI / sub2api 的 Antigravity 实现）。消费级 Google One 账号的旧 Gemini CLI 免费通道已被 Google 退役（sub2api 同样将新一代模型全部切到 Antigravity）。
- **`google/gemini-cli`**（driver key `google-gemini-cli`，别名 `gemini` / `gemini-cli` / `code-assist`）：Gemini CLI 公共 OAuth 客户端（`681255809395-…`）走 Code Assist 订阅（GCP Standard/Enterprise 层级，对齐 sub2api 的 `geminicli` 包）。与 antigravity 的差异：授权重定向到 `https://codeassist.google.com/authcode`（Google 页面展示裸授权码，用户复制粘贴；页面不回传 state，PKCE code_verifier 即会话绑定，state 仅在粘贴内容携带时校验）；请求为**精简信封** `{model, project, request}`（无 `requestType`/`userAgent`/`requestId` 指纹字段，不注入 `sessionId`，`safetySettings` 原样透传，`provider/google/gemini_cli.rs`）；UA 为 `GeminiCLI/0.1.5 (Windows; AMD64)`，不带 `x-goog-api-client`；onboardUser 走 prod host 且 body 为 `{tierId, metadata:{ideType:ANTIGRAVITY, platform, pluginType:GEMINI}}`；模型 id 干净无档位后缀（`gemini-3.6-flash` vs antigravity 的 `gemini-3.6-flash-high`）。

共享行为（两 channel 一致）：

- **认证**：PKCE S256 + `access_type=offline` + `prompt=consent`；exchange 时 `loadCodeAssist`/`onboardUser` 引导出 `cloudaicompanionProject` 并存入 credential meta（`project_id`/`email`/`tier_id`，refresh 时透传）。antigravity 的 UA 版本常量 `ANTIGRAVITY_VERSION`（≥2.9.1，Cloud Code 拒绝更低版本的新模型）需随 Google 客户端门控策略调整而 bump。
- **推理 host 分裂**（关键）：antigravity 通道的 `generateContent`/`streamGenerateContent` 走 **daily** host（`daily-cloudcode-pa.googleapis.com`，`ANTIGRAVITY_INFERENCE_BASE_URL`）——消费者（Google AI Pro/Ultra）凭证在 prod host 上会被扔进免费消费者池（共享 `aicode-consumers` 项目），主线模型全部被层级门控为 429 RESOURCE_EXHAUSTED；控制面（`loadCodeAssist`/模型发现）保持 prod（对齐 CLIProxyAPI `resolveAntigravityRequestBaseURL` 与 sub2api 的 paid→daily 规则）。gemini-cli 通道（GCP Code Assist）推理保持 prod。`bind_runtime` 的 `base_url_override` 承载该分裂，分发与探针均优先使用 override。
- **线格式**：响应与每条 SSE data 行均为 `{response:{…}}` 信封（antigravity 与 gemini-cli 相同），经 `pre_parse` / `on_stream_raw_chunk` 解包后走标准 google-gemini codec；antigravity 的请求侧额外包成 `{model, project, requestType:"agent", userAgent:"antigravity", requestId, request:{…Gemini 体…}}`（注入 `request.sessionId`、剥离 `request.safetySettings`）。
- **管道配合**：两个 channel 均声明 request/response mutations（`declared_*_mutations_for` 按 channel 判定，default channel 保持字节级直通）；`conversion/resolver.rs` 将其排除出 raw-wire compat（compat 路径不经过 vendor 响应钩子）；dispatcher 把 OAuth credential 贯通进 `ProviderCtx.credential` 供 vendor 钩子读取 project_id；流式路径在 IR 解码前调用 `on_stream_raw_chunk`（`StreamRawChunkHook`）。
- **用量显示**：`admin/usage.rs` 的 `google_subscription` 后端经 `v1internal:fetchAvailableModels` 读取按模型配额，**整个 provider 只报一行**（该后端专供 Gemini 流量）：① 思考档位变体（`-low/-medium/-high/-tiered`）先归入其**模型家族**（`gemini-3.8-flash` 家族取最紧的变体）；② 第一方 `gemini-*` 家族共享同一订阅桶（数值同步变化），折叠成单行 `gemini`（**共享池**，取**众数**作为桶值——自带独立配额的小众家族因此无法冒充桶值，也不会单独把 provider 判死）。第三方池（`claude-*` / `gpt-oss-*`）与旁路 Gemini 配额**不再成行**；仅当目录里没有任何第一方 `gemini-*` 条目时回退为按池列出，避免空快照被调度读成「无数据」。无 `quotaInfo` 的模型跳过，**内部噪音条目**（`chat_<digits>`、`tab_*_preview`——它们报配额但只服务 IDE 补全方言、不能承接 agent 请求，由 `is_subscription_usage_noise` 判定）在任何归并之前剔除；该判定**不并入** `is_unavailable_subscription_model`，发现/探测列表因此保持原状。`level` 显示凭据记录的订阅层级（`free-tier` / `g1-pro-tier` / `g1-ultra-tier`）。**调度**：这一行就是 `google_scheduling_observation` 的输入，故共享桶打满即暂停该 provider 的调度（对只跑 Gemini 的 Google 订阅后端即正确语义；`google_scheduling_observation` 取「余量最大的池」的既有逻辑对单行退化为原值）；OAuth 后端（codex/grok/google）在用量监控白名单内（无需 API key）。
- **effort → 档位模型改写**（pipeline 5f 步，`antigravity::apply_tier_model_rewrite`）：该表面思考档位编在模型 ID 里，客户端对基础 ID（如 `gemini-3.8-flash`）携带推理强度时自动改写为账号目录中真实存在的档位变体——词表映射：`none`/`disable`/`minimal`→`-extra-low`（缺失降级 `-low`）、`low`→`-low`、`medium`→`-medium`、`high`/`xhigh`/`max`→`-high`、无 effort→`-tiered`、budget 按量级映射；最近可用降级（如家族只有 `-low` 时 high 降级到 `-low` 而非 404）。**目录门控**：账号目录快照存于 credential meta `subscription_models`（登录/续期时由驱动 best-effort 抓取），基础 ID 本身就在目录中（如 `gemini-2.5-flash`）或档位变体不存在时不改写；改写后剥离内层 `thinkingConfig`（ID 即档位，避免双重指定）。
- **Gemini 3 思考签名（`thoughtSignature`）兼容**：流解码器按「含哪些键」分类 part（`thought` → ThinkingDelta、`text` → TextDelta、`functionCall` → 工具调用，`thoughtSignature` 经 ThinkingSignature 保留），**不再用「对象只有一个键」判定**——旧判定把带签名的 part 全归为 `Unknown`，而 OpenAI/Anthropic 出口丢弃 `Unknown`，导致纯思考或「思考+工具调用」的回合在客户端表现为「completed response with no content」。未知形状（含非 `thoughtSignature` 的额外字段）仍走 `Unknown` 逐字保留，Gemini → Gemini 往返不失真。**跨回合签名回放**（pipeline 5g 步，`antigravity::apply_thought_signature_policy`）：Gemini 3 要求多轮历史里的 `functionCall` 携带 `thoughtSignature`，而 OpenAI/Anthropic 客户端协议没有承载签名的字段——上游会 400 `Function call is missing a thought_signature in functionCall parts`。策略对齐 CLIProxyAPI `SanitizeGeminiRequestThoughtSignatures`：每个 model 轮次的**第一个** functionCall 缺失签名时填官方旁路哨兵 `skip_thought_signature_validator`；兄弟并行调用保持原生无签名形状（哨兵在非首个调用上非法，会被清除）；`functionResponse` 永不携带签名；已有真实签名原样保留。仅作用于 Google 订阅通道且模型为 Gemini 3+（`gemini-` 且非 `gemini-2`），Claude/gpt-oss/2.x 历史不受影响。
- **强制上游流式**：Code Assist v1internal 的非流式 action 可能返回空体——sub2api 与 CLIProxyAPI 均以 `:streamGenerateContent` 调上游再聚合。nyro 对两个 channel 无条件走流式上游（`antigravity::forces_upstream_stream` 翻转 IR stream 标志 → 聚合路径 `handle_non_stream_via_upstream_stream`，同样应用 `StreamRawChunkHook` 解包），非流式客户端收到聚合后的完整响应；admin 模型探测（probe）同样以流式 + SSE 提取实现（按 channel 选择信封）。
- **模型目录**：优先调用 `v1internal:fetchAvailableModels` 做**按账号动态发现**（订阅的真实目录，新模型先于此处任何静态表出现；失败时自动回退 curated 静态表 `ANTIGRAVITY_STATIC_MODELS` / `GEMINI_CLI_STATIC_MODELS`）；`deprecatedModelIds` 声明的条目在解析时剔除（上游自标已死、推理表面 404）；antigravity 通道另有 curated 不可用清单 `GEMINI_SUBSCRIPTION_UNAVAILABLE_MODELS`（退役 preview/image id、defunct id）+ `chat_<数字>` 内部噪音模式，经 `filter_subscription_unavailable` 从发现/探测列表过滤（手填模型名仍可路由，镜像 opencode-go 的 `UNAVAILABLE_MODELS` 语义；精选清单仅作用于 antigravity——相同 preview id 在 API-key/GCP 表面可能仍活）。配额（remainingFraction）接入探测结果展示。
- **WebUI**：google 预设下选择 channel 后，OAuth 会话以 channel 感知的 vendor key 发起（`google-gemini-cli`），provider 创建/重连时按 driver key 标记 `channel` 字段。

### 6.6 OpenCode Go（opencode.ai/zen/go）通道

`opencode-go` 是 Go 订阅（$10/月）的**三端点自适应**通道：上游把不同模型分发在 `/v1/chat/completions`、`/v1/responses`、`/v1/messages` 三个端点上，且对"该端点不服务此模型"的请求回**不透明的 500**（`Internal server error`）而非明确的 not-supported，因此端点归属无法从错误码推断——由硬编码表决定（`provider/opencode_go/routing.rs`）。

- **会话标识**（`provider/opencode_go/session.rs`）：上游对**每个请求**强制要求会话标识，缺失即 400 `{"type":"error","error":{"type":"MissingSessionID",…}}`（官方要求「为每个会话发送稳定的 `x-opencode-session`，以便优化路由与提示缓存」，opencode.ai/docs/go#where-can-i-use-it）。id 由会话首轮指纹（`system` + 首条 user 文本）派生成 `nyro-<16 字节 hex>`——会话增长时首轮不变，故同一会话各轮共享同一 id；无 user 文本（如仅工具结果的续轮）时回退随机 id。客户端自带 `x-opencode-session` 时**永不覆盖**。
- **会话标识注入点**：dispatcher 出站 header 汇合处（`proxy/dispatcher/mod.rs`，位于 passthrough / IR 编码 / raw-wire compat 三条构建路径之后、forwarded client headers 合并之后）——Claude Code（anthropic ingress 的 compat 路径）、Responses/OpenAI 客户端（passthrough）与转码路径全部覆盖。admin 模型探测（`admin/providers.rs`）按模型播种确定性 id；vision shim 直连 helper 的调用（`vision_shim/caption.rs`，不经 dispatcher）单独注入。
- **预设**：channel 声明三端点（同一 host `https://opencode.ai/zen/go`）+ `sharedKeyProtocols`（一个订阅 key 覆盖三端点，UI 因此按共享 Key 语义播种并锁定为自适应模式）+ `authSchemes` 里 `anthropic-messages → x-api-key`（`/v1/messages` 不吃 Bearer）。历史遗留的固定模式 provider 行不受影响，在 UI 打开→保存一次即升级为三端点自适应。
- **模型 → 端点硬编码表**（`routing.rs`，2026-09 实测 37 个 `/v1/models` 条目 × 3 端点）：
  - 三端点全可用：`deepseek-flash`、`deepseek-v4-flash`、`deepseek-v4-flash-vision-exp`、`deepseek-v4-pro`、`deepseek-v4.1-flash`；
  - chat + messages：`kimi-k3`、`minimax-m2.5`、`minimax-m3`、`qwen3.6-plus`、`qwen3.7-max`、`qwen3.7-plus`、`qwen3.8-flash`、`qwen3.8-max`；
  - 仅 responses：`gpt-5.6-luna`、`grok-4.6`、`muse-spark-1.2-contributor`、`muse-spark-1.3-contributor`；
  - 仅 messages：`minimax-m2.7`；
  - 其余（含未收录的新模型）**默认仅 chat**；精确匹配（trim + 大小写不敏感），按目标模型（`actual_model`）判定。
- **裁决规则**（`routing::preferred_egress` → `negotiate()` 的 route_pref）：**客户端协议优先**——模型支持客户端协议就原生直通（Claude Code 问 `kimi-k3` 走 `/v1/messages`，Codex 问 `deepseek-*` 走 `/v1/responses`）；不支持才改道到该模型的端点（chat 客户端问 `grok-4.6` → `/v1/responses` 转码；Claude Code 问 `glm-5.3` → `/v1/chat/completions` 转码，否则自适应协商会选中 messages 端点并 500）。偏好只在 provider 真的声明了该端点时生效，固定 provider 优雅退化为原有行为；不做端点级失败回退（端点选定即定，上游错误照原样透出）。
- **已知不可用黑名单**（`routing.rs::UNAVAILABLE_MODELS`）：`glm-5`、`grok-4.5`、`hy3-preview`、`kimi-k2.5`、`mimo-v2-omni`、`mimo-v2-pro`、`qwen3.5-plus` 仍出现在上游 `/v1/models` 里，但订阅内每个端点都不可用。它们被**从 provider 模型列表中过滤**（选择器/探测/路由目标都看不到），手填模型名仍可绕过，因此上游若恢复服务不会被代码堵死。
- **模型探测**（`admin/providers.rs::resolve_probe_target`）：与转发共用同一决策——每个模型探它实际会被路由到的端点（`grok-4.6` → responses、`minimax-m2.7` → messages、其余 chat），因此"探测绿"等价于"经网关可调用"；逐模型的实际端点回填到 `ProviderModelProbeResult.protocol`，前端按模型显示 `[协议]`。

---

## 7. 错误处理

`GatewayError` 统一 taxonomy：

| 变体 | HTTP | 含义 |
|---|---|---|
| `BadRequest` | 400 | 客户端格式错误 |
| `Unauthorized` | 401 | 无有效 API Token |
| `Forbidden` | 403 | Token 状态异常或无权限 |
| `QuotaExceeded` | 429 | RPM / TPM / TPD 超限 |
| `RouteNotFound` | 404 | 无匹配模型/路由 |
| `ProtocolUnsupported` | 400 | 协议不支持 |
| `ProtocolLossyRejected` | 422 | lossy 转换被拒绝 |
| `ProviderUnavailable` | 503 | 无可用 vendor extension |
| `UpstreamStatus` | 上游 status | 上游返回错误 |
| `UpstreamTimeout` | 504 | 上游超时 |
| `StreamParseError` | 502 | SSE chunk 解析失败 |
| `ClientCancelled` | 499 | 客户端断开 |
| `Internal` | 500 | 内部错误 |

每个错误由 `GatewayError::render(request_id)` 统一序列化为 OpenAI 兼容 JSON 错误格式。

---

## 8. 模型（路由）与访问控制

### 8.1 Model 模型（原 Route）

模型唯一键为 `name`，客户端请求中的 `model` 值与之精确匹配即命中：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | TEXT PK | UUID |
| `name` | TEXT | 显示名称，同时作为模型匹配键 |
| `balance` | TEXT | 负载策略：`weighted` / `priority` / `latency` / `usage`（最大周期窗口剩余额度÷剩余时间比率 r³ × 窗口易损性加成 `(30d/W)^0.5`，月=1、周≈2.07、5h 封顶 4） |
| `target_provider` | TEXT FK | 默认目标 Provider（兜底）|
| `target_model` | TEXT | 默认上游模型名 |
| `enable_auth` | BOOL | API Token 访问控制，默认 false |
| `enable_payload` | BOOL/NULL | 是否记录 payload；NULL 时跟随全局开关 |
| `is_enabled` | BOOL | 模型启用状态，默认 true |

> `ingress_protocol` 不在数据库中。协议在运行时由 `RequestContext` 携带，日志写入 `request_logs.client_protocol`。

**后端列表（model_backends）**：一个 Model 可绑定多个 backend，每个 backend 指向 `provider_id` + `model`，带 `weight`（weighted balance；usage 下拆分同 Provider 的 target）和 `priority`（priority balance）；后端健康状态在内存 `HealthRegistry` 管理，不入库。`ProviderQuotaRegistry` 同时保留最近一次权威用量窗口快照，供 usage balance 在请求热路径同步读取。

**降级兜底（is_fallback）**：每个 Model 可将恰好一个 backend 行标记为降级兜底（`is_fallback=1`）。该行不参与任何 balance 策略，由 `TargetSelector` 统一追加在有序目标列表末尾——只有当所有正常目标被跳过（配额耗尽/熔断打开/provider 禁用）或可重试失败（408/429/5xx/529）后才会被调用；非可重试 4xx 仍立即返回客户端。兜底行同样受配额/熔断闸门约束（配额按 provider 生效，建议兜底放在不同 provider）。路由决策快照中该行记为 `state: "fallback"`、rank 排最后，被闸门跳过时同样记录 skip 原因。

### 8.2 API Token 模型

Model 与 API Token 是**独立管理、多对多绑定**的关系（经 `api_key_models` 表）：

```
API Token ──── (授权绑定) ──── Model
  │                             │
  ├── 配额: RPM / RPD / TPM / TPD  ├── 匹配键 (name)
  ├── 过期时间                  ├── 后端列表 (model_backends)
  ├── 状态: is_enabled           ├── 负载策略 (balance)
  └── 名称                       └── 访问控制 (enable_auth)
```

Token 格式：`sk-<32位hex>`（存储字段名 `token`）。

### 8.3 代理请求鉴权流程

```
1. 解析请求 → 提取 model, api_token
   (优先级: Authorization: Bearer > x-api-key)
2. match(model) → models 表精确匹配 name
   └── 未匹配 → GatewayError::RouteNotFound (404)
3. if model.enable_auth == false:
   └── 直接放行
4. if api_token 为空 → GatewayError::Unauthorized (401)
5. 验证 api_token:
   a. 不存在 → 401 invalid token
   b. is_enabled == false → 403 token revoked
   c. expires_at < now → 403 token expired
   d. model 不在 token 绑定列表（api_key_models）→ 403 forbidden
   e. 配额超限 (rpm / tpm / tpd) → GatewayError::QuotaExceeded (429)
6. 执行路由转发 → model_backends → 健康感知 target 选择
```

---

## 9. 模型能力识别

### 9.1 ai:// 内部协议

Provider 配置中通过 `modelsSource` / `capabilitiesSource` 声明数据来源：

| 值类型 | 示例 | 说明 |
|---|---|---|
| HTTP URL | `https://api.openai.com/v1/models` | 直接向 HTTP 端点请求 |
| 内部协议 | `ai://models.dev/openai` | 从 Nyro 内嵌 / 缓存的 models.dev 数据中查询 |

### 9.2 VendorMetadata

每个厂商通过 `const METADATA: VendorMetadata`（位于 `provider/<vendor>/mod.rs`）声明，由 `VendorRegistry::list_metadata_for_webui()` 聚合输出给 WebUI。

---

## 10. 存储与数据层

### 10.1 多后端

| 后端 | 适用形态 | 路径 |
|---|---|---|
| SQLite | Desktop（单用户本地） | `crates/nyro-core/src/storage/sqlite/` |
| PostgreSQL | Server（多用户自托管） | `crates/nyro-core/src/storage/postgres/` |
| MySQL | Server（多用户自托管） | `crates/nyro-core/src/storage/mysql.rs` |
| Memory | 测试 / mock | `crates/nyro-core/src/storage/memory.rs` |

统一接口定义在 `crates/nyro-core/src/storage/traits.rs`，上层代码不感知具体后端。
权威 Schema 文档：[docs/database/schema.md](../database/schema.md)（含 `deploy/schema/postgres.sql` / `mysql.sql`）。

### 10.2 核心表结构（最终态，post-migration）

> 历史迁移：`routes` → `models`，`route_targets` → `model_backends`，`api_key_routes` → `api_key_models`，`api_keys.key` → `api_keys.token`。

```sql
-- 提供商配置
CREATE TABLE providers (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    vendor          TEXT,             -- canonical vendor_id
    protocol        TEXT NOT NULL,
    base_url        TEXT NOT NULL,
    api_key         TEXT NOT NULL,    -- static api key
    auth_mode       TEXT NOT NULL,
    use_proxy       INTEGER NOT NULL DEFAULT 0,
    is_enabled      INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- 模型（路由规则）
CREATE TABLE models (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,  -- 匹配键 + 显示名
    balance         TEXT NOT NULL DEFAULT 'weighted',
    target_provider TEXT NOT NULL REFERENCES providers(id),
    target_model    TEXT NOT NULL,
    enable_auth     INTEGER NOT NULL DEFAULT 0,
    enable_payload  INTEGER,               -- NULL = 跟随全局
    is_enabled      INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT NOT NULL
);

-- 模型后端列表
CREATE TABLE model_backends (
    id          TEXT PRIMARY KEY,
    model_id    TEXT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL REFERENCES providers(id),
    model       TEXT NOT NULL,        -- 上游实际模型名
    weight      INTEGER NOT NULL DEFAULT 100,
    priority    INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL
);

-- 访问控制 Token
CREATE TABLE api_keys (
    id         TEXT PRIMARY KEY,
    token      TEXT NOT NULL UNIQUE,  -- sk-<32位hex>
    name       TEXT NOT NULL,
    rpm        INTEGER,
    rpd        INTEGER,
    tpm        INTEGER,
    tpd        INTEGER,
    is_enabled INTEGER NOT NULL DEFAULT 1,
    expires_at TEXT
);

-- Token 与 Model 的绑定关系
CREATE TABLE api_key_models (
    api_key_id TEXT NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    model_id   TEXT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    PRIMARY KEY (api_key_id, model_id)
);

-- 请求日志（append-only，快照，无 FK）
CREATE TABLE request_logs (
    id                        TEXT PRIMARY KEY,
    created_at                INTEGER NOT NULL,  -- Unix 毫秒
    api_key_id                TEXT,
    api_key_name              TEXT,
    client_protocol           TEXT,              -- ingress 协议
    upstream_protocol         TEXT,              -- egress 协议
    provider_id               TEXT,
    provider_name             TEXT,
    model_id                  TEXT,
    model_name                TEXT,
    upstream_url              TEXT,
    client_model              TEXT,
    upstream_model            TEXT,
    method                    TEXT,
    path                      TEXT,
    upstream_status_code      INTEGER,
    client_status_code        INTEGER NOT NULL,
    latency_total_ms          INTEGER,
    latency_upstream_ms       INTEGER,
    input_tokens              INTEGER,
    output_tokens             INTEGER,
    cache_read_tokens         INTEGER,
    is_stream                 INTEGER,
    stream_chunks_count       INTEGER,
    stream_first_chunk_ms     INTEGER,
    -- payload（仅 enable_payload=true 时填充）
    client_request_headers    TEXT,
    client_request_body       TEXT,
    client_response_headers   TEXT,
    client_response_body      TEXT,
    upstream_request_headers  TEXT,
    upstream_request_body     TEXT,
    upstream_response_headers TEXT,
    upstream_response_body    TEXT
);

-- 全局配置 KV
CREATE TABLE settings (
    name       TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- OAuth 凭据（与 providers 1:1）
CREATE TABLE provider_oauth_credentials (
    provider_id    TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
    driver_key     TEXT NOT NULL,
    scheme         TEXT NOT NULL,
    access_token   TEXT,
    refresh_token  TEXT,
    expires_at     TEXT,
    status         TEXT NOT NULL,
    last_error     TEXT,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);
```

> 后端健康状态（熔断 / 成功率）在运行时内存 `HealthRegistry`（`router/health.rs`）管理，**不持久化到数据库**。

### 10.3 安全

- Desktop 模式下管理 API 仅监听 `127.0.0.1`，外部不可访问
- Server 模式下管理端口与代理端口独立，可配置鉴权

---

## 11. 前端适配层

前端（`webui/`）通过薄抽象层兼容两种部署形态（`webui/src/lib/backend.ts`）：

- **Desktop 版**：通过 Tauri IPC（`invoke(cmd, args)`）调用
- **Server 版**：通过 HTTP 调用（`fetch(/api/v1/...)`）

**技术栈：**

| 层 | 技术 |
|---|---|
| 框架 | React 19 + TypeScript + Vite |
| 状态 | Zustand |
| 数据获取 | TanStack Query |
| 路由 | React Router v7 |
| 样式 | Tailwind CSS 4 |
| 图表 | Recharts |

---

## 12. 未实施能力 / Future Work

### 12.1 Pass-Through 路径 ✅ 已实现

当 ingress/egress 协议一致且 `Vendor` 声明无 mutation 时，dispatcher 可走 PassThrough。请求侧绕过跨协议 IR 转码，但仍以解析后的 JSON 应用 actual model、协议默认值与必要的安全归一化；响应侧在无 mutation 时可直接转发上游 body / SSE 字节。因此 PassThrough 表示“不做跨协议语义转换”，不承诺请求字节逐字节不变。

### 12.2 Quota 预留与结算

当前 quota 检查仅在请求前执行，并发场景存在超额风险。建议改为：preflight 估算 → atomic 预留 → 执行请求 → settle 实际用量 → refund 未消费预留。stream 客户端 cancel 时也需结算已产生的 token。

### 12.3 Fixture 契约测试体系

```
tests/fixtures/protocol/
  openai_chat/ · openai_responses/ · anthropic_messages/ · google_generate/

tests/contract/
  openai_chat_to_anthropic.rs  anthropic_to_openai_chat.rs  ...

tests/stream/
  normal_done.rs  upstream_disconnect.rs  malformed_chunk.rs
  client_cancel.rs  usage_in_final_chunk.rs
```

### 12.4 Compatibility Matrix CI

自动化验证每个 ingress→egress protocol 组合的支持程度（Native / Transform / LossyTransform / Reject），在 CI 生成兼容性报告，防止回归。

### 12.5 Record-Replay

捕获真实上游请求/响应 pair，存为 fixture，用于离线复现 bug、provider 更新后兼容性测试、流式异常场景精确重放。

### 12.6 可观测性 Exporter（Plugin-Level）

OnLog 阶段 + `ResponseStats` 已提供标准化的请求指标消费点（见 §4）。下一步：通过 `PhaseHook`（`OnLog`）实现 exporter 插件，输出 trace / metrics / logs 到 OTel Collector / Jaeger / Prometheus / Grafana 等平台。详见 [docs/design/lifecycle.md](lifecycle.md)。

### 12.7 长尾厂商适配

当前覆盖主流厂商（OpenAI / Anthropic / Google / Vertex AI / DeepSeek / Moonshot / GLM (zhipuai) / MiniMax / xAI / ZAI / OpenRouter / Nvidia / Ollama）。待补充：
- AWS Bedrock（SigV4 签名 + wrapper protocol）
- Azure AI Foundry（Azure AD token + deployment URL pattern）
- Cohere / Mistral / Together AI 等

### 12.8 Router 故障策略（部分已落地）

已落地：多 backend 健康感知迭代（`HealthRegistry`）+ 四种 `balance` 策略 + 可重试状态码自动续跑 + 每模型单行降级兜底（`model_backends.is_fallback`，末位追加、仅在正常目标全部不可用时调用）。`latency` 由内存 `LatencyRegistry` 按流式首字延时 EWMA 排序，目标需连续 3 个流式样本并以三次均值入组，未满或超过 5 分钟保鲜窗时由真实流量乐观探测。`usage` 由 `ProviderQuotaRegistry` 的 last-good 窗口快照驱动：每个 Provider 只按其最大主窗口（月>周>5h）计算所需加速 `r=剩余额度%÷剩余时间%`，全部可评分 Provider 在单一池内按 `r³` × 窗口易损性加成 `(30d/W)^0.5`（月=1、周≈2.07、5h 封顶 4；短窗配额更易作废且透支恢复更快，故安全承载更大份额）加权随机（r>1 加速烧、r<1 让位、临近重置自动放大、上限 10）；未知用量 Provider 只作末级兜底，Provider 间等权、Provider 内按静态 target 权重排列。任何窗口达到 100% 仍触发配额硬过滤。待补充：指数退避 + jitter、可配置重试上限、单 backend 精细化熔断（滑动窗口）。

### 12.9 Transport 策略

- HTTP/2 上游连接（降低延迟，复用连接）
- 连接池配置（per-provider max connections）
- 请求级超时精细化（connect_timeout / read_timeout / total_timeout 分离）
- 可配置重试策略
