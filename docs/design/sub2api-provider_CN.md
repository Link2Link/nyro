# Sub2API 供应商：按分组平台完整对接（设计）

> 状态：**P1 已实现**（§8）；P2 已实现拒绝处理与平台识别（§9；已移除上游 `/v1/usage` 余额/额度轮询与调度暂停，Sub2API 始终仅保留本地计费统计）；其余 P2 项与 P3 尚未实现。基线：nyro v2.1.57（`6674406`）；sub2api `81ce8356d`（本机 `~/code/sub2api`）。
> English: [sub2api-provider.md](sub2api-provider.md)

## 1. 背景、目标与非目标

**现状**

- nyro 只有一个 `openai/sub2api` 渠道（[openai/mod.rs](../../crates/nyro-core/src/provider/openai/mod.rs)）：固定 `openai-responses`，复用 Codex 消费级出站契约（`CodexOAuthResponses` flavor、参数剥离、reasoning 防御、工具 ID 规范化、Fast 档），用量只有本地日志统计（`LocalUsageAccounting::Sub2Api`）。
- sub2api 的其他分组平台只能借 custom / anthropic / google 预设手工拼地址，常见问题：
  - Antigravity 必须走 `/antigravity` 路由前缀，Gemini 原生接口在 `/v1beta`，地址和协议对不上就 404 / 400；
  - google 预设默认 `?key=` 鉴权，sub2api 的 `/v1/*` 直接拒绝（400 `api_key_in_query_deprecated`）；
  - 不清楚哪些协议在 sub2api 侧原生转发、哪些依赖分组开关（`allow_messages_dispatch`、仅限 Claude Code），导致难以解释的 403；
  - 看不到 sub2api 的真实额度 / 余额 / 订阅窗口，额度耗尽前无法提前切到其他后端；
  - 用量后端的 URL 子串探测（`z.ai`、`api.kimi.com`、`token-plan.` 等）可能把中转站地址误判为厂商直连。

**目标**：新增独立供应商预设 **Sub2API**（vendor `sub2api`）。用户只需 ① 选择平台（即 sub2api 分组的平台类型）② 填写 Sub2API 地址 ③ 填写 API Key。nyro 自动处理协议端点和路由前缀、统一鉴权、模型发现、协议协商与转换方向、上游方言策略、额度查询与调度，以及错误诊断提示。

**非目标**

- TypeSafe / Jev（只有 `/v1/systemone`，不是对话协议）；
- 图片、视频、语音、Realtime、Responses WebSocket、`/v1/messages/count_tokens`（nyro 尚无对应入口）；
- sub2api 管理面 API（需要用户 JWT，API Key 无法调用）；
- 伪造客户端身份绕过分组策略（例如「仅限 Claude Code」）。

## 2. sub2api 对外接口调研结论

### 2.1 分组平台

sub2api 的 API Key 绑定一个分组，每个分组有唯一的平台（`backend/internal/domain/platforms.go`）。请求进入哪条网关管线，由分组平台决定（`backend/internal/server/routes/gateway.go`）。

| 平台 id | 名称 | 网关族 | 上游 |
|---|---|---|---|
| `anthropic` | Anthropic | Anthropic 网关 | Claude OAuth / Setup Token / API Key / Bedrock / 上游透传 |
| `openai` | OpenAI | OpenAI 网关 | ChatGPT Codex OAuth / OpenAI API Key |
| `gemini` | Gemini | Anthropic 网关（Gemini 兼容服务） | AI Studio Key / Code Assist OAuth / Vertex |
| `antigravity` | Antigravity | Antigravity 网关 | Google v1internal（同时提供 Claude 与 Gemini 模型） |
| `grok` | Grok | OpenAI 网关 | xAI OAuth（cli-chat-proxy）/ xAI API Key |
| `kimi` `zhipu` `deepseek` `minimax` | 国产厂商 | OpenAI 网关（多协议 API Key 供应商） | 按账号 `api_protocol` 选择上游原生端点 |
| `opencode_go` `command_code` | 聚合平台 | OpenAI 网关 | 按模型分流到 chat / messages / responses |
| `cline` | Cline | OpenAI 网关 | 上游只有 Chat Completions |
| `composite` | 组合分组 | 按模型解析到具体平台 | 多平台 |
| `typesafe` | TypeSafe / Jev | — | 只有 `/v1/systemone`（**不支持**） |

### 2.2 端点 × 平台矩阵

图例：✅ 原生转发（sub2api 侧零转换）；🔁 sub2api 内部转换后可用；⚙️ 需要分组开关；❌ 不可用。

| 下游端点 | anthropic | openai | gemini | antigravity | grok | kimi / deepseek / minimax | zhipu | opencode_go / command_code | cline | composite |
|---|---|---|---|---|---|---|---|---|---|---|
| `POST /v1/messages` | ✅ | ⚙️🔁 `allow_messages_dispatch` | 🔁 | ✅（推荐 `/antigravity/v1/messages`） | 🔁 | ✅ | ✅ | ✅ / 🔁（按模型） | 🔁 | 按模型解析 |
| `POST /v1/chat/completions` | 🔁（仅限 Claude Code 的分组拒绝） | 🔁 / ✅ | 🔁 | 🔁 | ✅ | ✅ | ✅ | ✅ / 🔁 | ✅ | 按模型解析 |
| `POST /v1/responses` | 🔁 | ✅ | ❌（未发现 Gemini 账号的 Responses 转发路径） | 🔁 | ✅ | ✅ | 🔁 | ✅ / 🔁 | 🔁 | 按模型解析 |
| `POST /v1beta/models/{m}:{action}` | ❌ | ❌ | ✅ | ✅（只能走 `/antigravity/v1beta/...`） | ❌ | ❌ | ❌ | ❌ | ❌ | 解析到 Gemini 时可用 |
| `POST /v1/embeddings` | ❌ | ✅（API Key 账号） | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | 解析到 openai 时可用 |

说明：

- `/v1beta` 要求分组平台为 gemini，否则返回 400 `API key group platform is not gemini`；`/antigravity/*` 路由带 `ForcePlatform(antigravity)`，只调度 Antigravity 账号，不与其他账号混合。
- Antigravity 的 `/antigravity/v1/messages` 对 Claude 和 Gemini 模型都可用（sub2api 把 Claude 协议转换为 v1internal 后发出）；`/antigravity/v1/*` 下没有 chat / responses，这两类走根路径 `/v1/...`（按分组平台路由）。

### 2.3 鉴权

- `/v1/*`：`Authorization: Bearer` → `x-api-key` → `x-goog-api-key`；带 `?key=` 直接返回 400。
- `/v1beta/*`、`/antigravity/v1beta/*`：`x-goog-api-key` → Bearer → `x-api-key` → `?key=`。
- **结论：所有端点统一使用 Bearer。**

### 2.4 只读辅助端点（API Key 可调用）

| 端点 | 用途 | 备注 |
|---|---|---|
| `GET /v1/models` | 分组模型列表 | 三种格式：OpenAI（openai）、Grok（含 `reasoningEfforts`）、Claude（其余平台）；都包含 `data[].id` |
| `GET /v1beta/models` | Gemini 格式列表 | 只有 gemini 分组可用；账号未声明映射时会透传上游列表，比 `/v1/models` 的静态兜底更完整 |
| `GET /v1/usage` | 额度 / 余额 / 订阅 / 用量 | 三种模式，见 §3.9 |
| `GET /v1/sub2api/billing[?include=group]` | 计费倍率、分组名 | simple 模式返回 404；**不含平台字段** |

目前没有任何 API Key 可调用的接口直接返回分组平台，自动识别只能靠启发式规则（§3.10）。

### 2.5 sub2api 已负责的上游适配（nyro 不再重复）

- Claude OAuth：Claude Code 伪装、`metadata.user_id`、计费头；
- Antigravity：Claude / Gemini → v1internal 信封、身份提示词补丁、上游强制流式与非流式聚合、思考档位模型映射、思考签名注入与纠错重试；
- Gemini：注入 `thoughtSignature`（`ensureGeminiFunctionCallThoughtSignatures`），过滤空 parts；
- OpenAI OAuth：剥离 Codex 不支持的字段（`openai_codex_transform.go`）；
- OpenCode：注入 `x-opencode-session`；Grok：清洗 Responses 请求体；
- 账号粘性、并发排队、账号故障切换、计费。

### 2.6 需要向用户说明的约束

- **仅限 Claude Code 的分组**：sub2api 校验 UA `claude-cli/x.y.z`、system prompt 相似度、`X-App` 等（`claude_code_validator.go`）。只有真实的 Claude Code 经 nyro 原生直通时可用，nyro 不伪装客户端身份。
- **OpenAI 分组的 `/v1/messages`** 需要分组开启 `allow_messages_dispatch`（`openai_gateway_handler.go:310`）；grok、国产厂商与聚合平台不需要此开关。
- **Antigravity Claude 与 Anthropic Claude 不能在同一会话中混用**：两者的思考签名不同。
- **错误语义**：余额 / 订阅 / Key 总额度不足返回 **403 `billing_error`**；Key 的 5h / 1d / 7d 窗口、RPM 以及平台日 / 周 / 月配额超限返回 **429 `rate_limit_exceeded`**（部分带 `Retry-After`）；计费服务故障返回 503；没有可用账号时返回 503 / 502。

## 3. 总体设计

### 3.1 职责边界

- **nyro 负责客户端侧**：解析入口协议，选择出站协议与 sub2api 路由，做必要的协议转换，处理跨 provider 的路由、健康、额度调度和故障转移。
- **sub2api 负责账号侧**：账号池、上游伪装 / 信封 / 签名，以及计费。
- 对 sub2api provider，nyro **不运行**任何 OAuth、订阅信封、Claude Code cloak、Antigravity 档位改写或思考签名策略（这些都由 vendor 身份门控，sub2api vendor 永远不会命中）。

### 3.2 数据模型（不修改数据库 schema）

| 字段 | 取值 |
|---|---|
| `vendor` / `preset_key` | `sub2api` |
| `channel` | 平台 id，与 sub2api 保持一致（如 `antigravity`、`opencode_go`） |
| `protocol_mode` | `adaptive` |
| `protocol_endpoints` | 每个勾选的协议一行；`base_url` 都是规范化后的 Sub2API 根地址；同一个 `api_key`；`auth_scheme = bearer` |
| `base_url` | 根地址 |
| `models_source` | `{root}/v1/models`（gemini 平台用 `{root}/v1beta/models`，见 §3.8） |
| `fast_mode` | 只在 openai 平台生效 |
| `provider_keys` | 可选：同一平台的多 Key / 多站点池；Key 级 `base_url` 覆盖同样规范化为根地址 |

Standalone YAML 需要给 `YamlProvider` 增加可选的 `channel` 字段（[yaml_config.rs:43](../../src-server/src/yaml_config.rs#L43)）。

### 3.3 平台通道表（核心）

「默认勾选」只包含 sub2api 原生转发、且不依赖分组开关的协议；「可选」默认不勾选，勾选后改由 sub2api 转换。没有勾选的入口协议由 nyro 转换为默认协议。

| channel | 默认勾选 | 可选 | 默认协议 | 路由前缀 | 方言（§3.6） | 能力来源 | 提示 |
|---|---|---|---|---|---|---|---|
| `anthropic` | Messages | Chat、Responses | Messages | — | anthropic | models.dev `anthropic` | 仅限 Claude Code 的分组只接受真实 Claude Code 直通 |
| `openai` | Responses | Chat、Messages⚙️ | Responses | — | openai + Codex 契约 | `openai` | Messages 需要分组开启 allow_messages_dispatch；支持 Fast 档 |
| `gemini` | Gemini | Messages、Chat | Gemini | — | google（API 方言，不是订阅通道） | `google` | Responses 客户端由 nyro 转换为 Gemini |
| `antigravity` | Messages、Gemini | Chat、Responses | Messages | Messages / Gemini → `/antigravity` | 无（交给 sub2api） | Auto | 不要与 Anthropic 平台在同一会话混用 |
| `grok` | Responses、Chat | Messages | Responses | — | xai | `xai` | Claude Code 请求默认由 nyro 转为 Responses（XaiStrictResponses） |
| `kimi` | Chat、Messages、Responses | — | Chat | — | moonshotai | `moonshotai` | |
| `zhipu` | Chat、Messages | Responses | Chat | — | zhipuai | `zhipuai` | 上游没有原生 Responses |
| `deepseek` | Chat、Messages、Responses | — | Chat | — | deepseek | `deepseek` | |
| `minimax` | Chat、Messages、Responses | — | Chat | — | minimax | `minimax` | |
| `opencode_go` | Chat、Messages、Responses | — | Chat | — | opencode-go | `opencode-go` | sub2api 按模型选择端点 |
| `command_code` | Chat、Messages、Responses | — | Chat | — | 只用模型名规则 | Auto | 同上 |
| `cline` | Chat | Messages、Responses | Chat | — | 只用模型名规则 | Auto | 上游只有 Chat |
| `composite` | Chat、Responses、Messages、Gemini | — | Chat | — | 中继透明（同 nyro-gateway） | Auto | sub2api 按模型解析平台 |

取舍：转换总是只发生一次，要么在 nyro，要么在 sub2api。默认让 nyro 转换，原因是 nyro 的转换器经过 cc-switch 对齐审计，日志中能看到完整的转换证据，而且不受分组开关影响。对按模型分流的平台（opencode_go / command_code / composite），三到四种协议全部直通，转换交给 sub2api，这样任何入口最多只转换一次。

### 3.4 URL 规范

```
endpoint_url = sub2api_root(base_url) + platform.route_prefix(protocol) + encoder_egress_path
```

- `encoder_egress_path` 是编码器给出的标准路径：`/v1/messages`、`/v1/chat/completions`、`/v1/responses`、`/v1/embeddings`、`/v1beta/models/{m}:generateContent` 或 `:streamGenerateContent?alt=sse`。
- `sub2api_root`：去掉末尾斜杠，再去掉一个已知后缀（不区分大小写，按顺序匹配）：`/antigravity/v1beta`、`/antigravity/v1`、`/antigravity`、`/backend-api/codex`、`/v1beta`、`/v1`。反向代理的路径前缀会保留；主机名本身不会被当作路径段（与 [nyro_gateway::gateway_root](../../crates/nyro-core/src/provider/nyro_gateway/mod.rs) 规则一致）。
- 后端在保存时规范化 `base_url`、每个端点的 `base_url` 和 Key 池中的 `base_url`；vendor 的 `build_url` 在运行时再规范化一次，作为防御。

| 用户填写 | 根地址 |
|---|---|
| `https://s.example.com/v1` | `https://s.example.com` |
| `https://s.example.com/antigravity` | `https://s.example.com` |
| `https://s.example.com/antigravity/v1beta/` | `https://s.example.com` |
| `https://edge.example.com/sub2api/v1` | `https://edge.example.com/sub2api` |
| `http://v1` | `http://v1` |

| channel | 出站协议 | 最终 URL（root = `https://s.example.com`） |
|---|---|---|
| antigravity | Messages | `https://s.example.com/antigravity/v1/messages` |
| antigravity | Gemini（流式） | `https://s.example.com/antigravity/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse` |
| antigravity | Chat（可选） | `https://s.example.com/v1/chat/completions` |
| gemini | Gemini | `https://s.example.com/v1beta/models/gemini-2.5-pro:generateContent` |
| openai | Responses | `https://s.example.com/v1/responses` |

鉴权：channel 元数据给所有协议声明 `auth_schemes = bearer`（同 nyro-gateway），避免 pipeline 把 Anthropic 出口改写成 `x-api-key`、把 Gemini 出口改写成 `?key=`。如果旧数据或 YAML 中出现 `auto`，sub2api vendor 也按 Bearer 处理。

### 3.5 请求流示例

**antigravity**

| 客户端入口 | nyro 协商结果 | 转换 | 上游 |
|---|---|---|---|
| Claude Code `/v1/messages` | 命中 Messages 端点 | 原生直通 | `/antigravity/v1/messages` |
| Gemini CLI `/v1beta/models/...` | 命中 Gemini 端点 | 原生直通 | `/antigravity/v1beta/models/...` |
| Codex `/v1/responses` | 未勾选 Responses，回落到默认 Messages | nyro：Responses → Messages（compat） | `/antigravity/v1/messages` |
| OpenAI SDK `/v1/chat/completions` | 未勾选 Chat，回落到默认 Messages | nyro：Chat → Messages | `/antigravity/v1/messages` |

**openai**

| 客户端入口 | 处理 | 上游 |
|---|---|---|
| Codex | Responses 直通 + Codex 契约 + Fast 档 | `/v1/responses` |
| Claude Code | Messages → Responses（compat，`CodexOAuthResponses`） | `/v1/responses` |
| OpenAI SDK（Chat） | Chat → Responses（IR） | `/v1/responses` |
| Gemini CLI | Gemini → Responses（IR） | `/v1/responses` |

### 3.6 方言映射（wire dialect）

新增 crate 内部的 `wire_dialect_vendor(provider)`：普通 provider 返回 `provider.vendor`；sub2api provider 返回平台对应的 nyro vendor id（见 §3.3「方言」列）。**它只用于线级策略：**

| 策略 | 现状 | sub2api 下的取值 |
|---|---|---|
| `apply_vendor_effort_policy`（[pipeline.rs:430](../../crates/nyro-core/src/provider/common/pipeline.rs#L430)） | 按 `provider.vendor` | 平台方言（openai 平台的 off → `low`，grok 平台删除 none、max → xhigh，opencode_go 平台 off → low，……） |
| Codex 消费级契约（参数剥离 / reasoning 防御 / 工具 ID / Fast，只作用于 Responses 出口） | `codex`，以及只看渠道的 `sub2api` | 增加 `sub2api/openai` |
| compat Anthropic → Responses flavor | codex / sub2api → `CodexOAuthResponses`；xai → `XaiStrictResponses` | openai → Codex；grok → XaiStrict；其余 → Standard |
| Responses → Responses 第三方改写 | 不含 openai / codex / sub2api / xai | openai：不改写；grok：走 xAI 原生路径；composite：不改写（中继透明）；其余平台按第三方规则 |
| Anthropic → Anthropic 的 DeepSeek 历史回放归一化 | deepseek / mimo | deepseek 平台（需联调确认） |

**禁止**把方言用于身份判断：OAuth driver、runtime binding、`is_google_antigravity` / `is_google_gemini_cli`、Claude Code egress、用量后端、账号池、订阅信封、`apply_tier_model_rewrite`、思考签名策略。这些由 sub2api 负责，或者根本不适用。

vendor 自身的 `post_encode` 等钩子（例如 MiniMax 的推理默认值）不对 sub2api 执行，只复用 pipeline 层的方言策略。

### 3.7 兼容旧的 `openai/sub2api` 渠道

- 统一判定函数 `Sub2ApiPlatform::of(provider)`：`sub2api/<平台>` 返回对应平台；`openai/sub2api` 与历史测试中出现的 `custom/sub2api` 都返回 `OpenAI`。所有 Codex 契约、Fast 档、用量判定都只调用这一个函数，避免现在各处散落的 `channel == "sub2api"` 判断。
- 旧渠道保留在 OpenAI 元数据中并标记 `deprecated`：新建 provider 时隐藏，编辑已有 provider 时正常显示，并给出「迁移到 Sub2API 预设」按钮（vendor / preset 改为 `sub2api`，channel 改为 `openai`，`base_url` 规范化为根地址，固定协议改为单 Responses 端点的 adaptive，保留 Key、Fast 档，`models_source` 改为 `{root}/v1/models`）。两者经由同一判定函数，行为一致，回归测试对两种写法参数化。
- 不在启动时自动迁移，避免改动用户自定义过的地址（例如 `/backend-api/codex`）。

### 3.8 模型发现与能力

- 模型发现**一律使用 Bearer**。现状 [build_model_headers](../../crates/nyro-core/src/admin/model_catalog.rs#L104) 在 Gemini 协议且 vendor 不是 google 时不发送任何鉴权头，sub2api 的 `/v1/*` 也拒绝 `?key=`，因此 sub2api 需要单独分支。
- gemini 平台用 `{root}/v1beta/models`（`models[].name`，现有解析会去掉 `models/` 前缀）；antigravity 和其余平台用 `{root}/v1/models`（包含账号映射中的 Claude 与 Gemini 模型）。
- 能力来源按 §3.3 表格中的「能力来源」列取；opencode-go、antigravity 订阅通道的可见性过滤由身份门控，不作用于 sub2api（sub2api 返回的列表已按分组过滤）。

### 3.9 用量与调度（新增后端 `sub2api_key`）

请求 `GET {root}/v1/usage`（Bearer，超时 20s）；P3 可选再请求 `GET {root}/v1/sub2api/billing` 读取倍率，404 时忽略。

| sub2api 字段 | nyro `ProviderUsage` |
|---|---|
| `mode=quota_limited`：`quota{limit, used, remaining}` | tier `key_quota`（used / limit，没有重置时间） |
| `rate_limits[{window: 5h / 1d / 7d, limit, used, reset_at}]` | `five_hour` / `daily` / `weekly_limit`，`resets_at = reset_at` |
| `status`（active / quota_exhausted / expired）、`isValid` | `is_available` |
| `expires_at`、`days_until_expiry` | 卡片上的到期信息 |
| `mode=unrestricted` + `subscription{daily/weekly/monthly_usage_usd, *_limit_usd, weekly_window_start, expires_at}` | 配置了 limit 的窗口生成 `daily` / `weekly_limit` / `monthly`；周窗口 `resets_at = weekly_window_start + 7d`；`level = planName` |
| 订阅 `remaining`（0 = 已耗尽，-1 = 不限） | `is_available` |
| 钱包模式 `balance` | `balances[{currency: USD, total}]`；`is_available = balance > 0` |
| `usage.today.actual_cost`、`usage.total.actual_cost` | `spends[today, total]` |
| 本地日志 | `api_price`（沿用本地统计，按自然月） |

日窗口按 sub2api 配置时区的 0 点刷新；月窗口是 `monthly_window_start + 30d`，但 `/v1/usage` 不返回月窗口起点，所以这两类窗口的 `resets_at` 留空。

调度规则（与现有后端一致）：

- `is_available = false` 时暂停调度，按常规监控周期复查；
- 主窗口达到 100% 且有 `resets_at` 时，暂停到重置时刻；`key_quota` 达到 100% 时暂停并定期复查；
- 查询失败或数据未知时保持可调度；
- 被动观测：遇到 403 `billing_error` 或 429 `rate_limit_exceeded` 时，故障转移到下一个目标，不计入本地健康失败，并触发一次限频的用量刷新。

识别方式：`UsageBackend::detect` 在 URL 兜底（[usage.rs:523](../../crates/nyro-core/src/admin/usage.rs#L523)）之前**按身份**识别 sub2api（新 vendor 与旧渠道都算），WebUI 的 [provider-usage.ts](../../webui/src/lib/provider-usage.ts) 同步。如果 `/v1/usage` 返回 404 或不是 JSON（旧版 sub2api），降级为现在的本地统计卡片（kind `sub2api`），调度不受影响。

### 3.10 连接测试、平台识别与错误提示

**连接测试**（`test_provider` 增加 sub2api 分支，不消耗 token）：

1. `GET /v1/usage`：校验 Key，读取分组名；401 提示「API Key 无效」。
2. 逐个检查已启用的端点：Gemini 端点请求 `GET {root}{prefix}/v1beta/models`，返回 400「not gemini」即提示「平台不符」；其他端点请求 `GET /v1/models` 并检查返回格式。
3. 「测试模型」探测走 vendor 的 URL 构造和 Bearer（现状 [providers.rs:282](../../crates/nyro-core/src/admin/providers.rs#L282) 直接调用 `openai_build_url`，会丢掉 `/antigravity` 前缀）。

**平台识别**（P2，「识别平台」按钮，只给建议，由用户确认）：

| 信号 | 结论 |
|---|---|
| `GET /v1beta/models` 返回 200 | gemini（或 composite） |
| `/v1/models` 是 OpenAI 格式（`owned_by: openai`） | openai |
| `/v1/models` 是 Grok 格式（`reasoningEfforts` / `owned_by: xai`） | grok |
| Claude 格式，同时有 `claude-*` 和带档位后缀的 `gemini-*` / `gpt-oss-*` / `tab_flash_lite_preview` | antigravity |
| Claude 格式，只有 `claude-*` | anthropic |
| `deepseek-*` / `kimi-*` / `glm-*` / `MiniMax-*` 只出现一家 | 对应国产平台 |
| 多家模型混合 | composite / opencode_go / command_code / cline（需要用户选择） |

保存时如果所选平台与识别结果不一致，给出警告。可选的上游增强：给 sub2api 的 `BillingGroupInfo` 增加 `group_platform`，nyro 读到该字段时直接精确识别，读不到再退回启发式规则。

**错误分类**（`provider/sub2api/errors.rs`，在 non_stream / stream / compat 三条上游错误出口统一调用）：

| 上游响应 | 含义 | nyro 处理 |
|---|---|---|
| 403 `billing_error` | 余额 / 订阅 / Key 额度不足 | 允许故障转移；本地健康中性；触发用量刷新；没有其他目标时把原错误返回客户端 |
| 429 `rate_limit_exceeded` | Key 窗口 / RPM / 平台配额 | 现有 429 逻辑 + 用量刷新；遵守 `Retry-After` |
| 403 `restricted to Claude Code clients` | 仅限 Claude Code 的分组 | 不重试；健康中性；提示原因 |
| 403 `does not allow /v1/messages dispatch` | OpenAI 分组未开启分发 | 提示取消勾选 Messages（改由 nyro 转换） |
| 400 `API key group platform is not gemini`、404 `... not supported for this platform` | 平台与分组不符 | 提示修改平台；模型探测标记失败 |
| 403 `model ... not in whitelist` | 分组不包含该模型 | 不重试；健康中性 |
| 503 `No available accounts`、502 `Upstream request failed after retries` | sub2api 账号池暂时不可用 | 可重试 / 故障转移（现有逻辑） |

### 3.11 会话粘性（P2）

- **OpenAI 网关族平台**（openai、grok、国产厂商、聚合平台、composite）：客户端没有携带 `session_id` / `conversation_id` / `x-session-affinity` / `x-session-id` / `x-opencode-session` / `x-conversation-id`，请求体也没有 `prompt_cache_key` 时，在 dispatcher 的出站 header 汇合点注入 `x-session-affinity: nyro-<hex>`。该值由 nyro API Key 与会话首轮指纹派生，算法同 `opencode_go::session`。sub2api 会把它当作第 3 优先级的粘性键，同一会话固定到同一账号，从而命中上游提示缓存。
- **Anthropic 网关族平台**：sub2api 以 `metadata.user_id` 或 `cache_control` 内容作为粘性键。nyro 不合成 `metadata.user_id`，否则会改变 sub2api 对 Claude Code 客户端的判定。P3 可选：转换到 Messages 时自动设置缓存断点。

### 3.12 WebUI

- 新增预设卡片 **Sub2API**。平台按三组展示：订阅平台（Anthropic / OpenAI / Gemini / Antigravity / Grok）、国产厂商（Kimi / 智谱 / DeepSeek / MiniMax）、聚合平台（OpenCode / Command Code / Cline / Composite）。
- 把 nyro-gateway 的地址字段泛化为「中继地址」组件，由元数据驱动，不再硬编码 `isNyroGatewayPreset`（[providers.tsx:2111](../../webui/src/pages/providers.tsx#L2111)）。占位符为 `https://<sub2api-host>`，说明中注明可以粘贴带 `/v1`、`/antigravity`、`/v1beta` 的地址。
- 协议勾选器：默认勾选原生协议，可选协议默认不勾选；每行显示最终路由（如 `…/antigravity/v1/messages`）；可选协议标注「由 Sub2API 转换」或「需要分组开关」。平台选择器下方显示该平台的提示。
- Fast 档只对 `sub2api/openai` 与旧渠道显示（[provider-speed-mode.ts:9](../../webui/src/lib/provider-speed-mode.ts#L9) 改为同时判断 vendor）。
- 用量卡片：kind `sub2api_key`，标题「Sub2API 额度」，展示窗口、余额、消费和本地等价价格。
- 编辑旧 `openai/sub2api` 渠道时显示迁移提示。所有新文案默认英文，另附中文（AGENTS 约定）。

## 4. 实现清单

### 4.1 新增

- `crates/nyro-core/src/provider/sub2api/`：`mod.rs`（`Sub2ApiVendor`、13 个通道的 `METADATA`、`is_sub2api`）、`platform.rs`（`Sub2ApiPlatform`：路由前缀、方言、能力）、`url.rs`（`sub2api_root`、`endpoint_url`）、`usage.rs`、`errors.rs`、`detect.rs`（P2）。
- `webui/src/lib/relay-address.ts`（从 `nyro-gateway.ts` 泛化而来）、`webui/src/lib/sub2api.ts`、图标（sub2api 仓库是 LGPL-3.0，建议用通用中转图标，或确认 logo 授权后再使用）。

### 4.2 元数据扩展（[metadata.rs](../../crates/nyro-core/src/provider/metadata.rs)）

在 `ChannelDef` 中增加以下字段，并提供 `ChannelDef::DEFAULT` 常量；现有字面量一次性改为 `..ChannelDef::DEFAULT`，属于机械修改。

- `relay: Option<RelayAddressing { models_path, route_prefixes, root_suffixes }>`
- `optional_protocols: &[&str]`
- `deprecated: bool`
- `notes: Option<Label>`

nyro-gateway 同步改用 `relay`。WebUI 的类型定义在 [types.ts:791](../../webui/src/lib/types.ts#L791)。

### 4.3 需要修改的调用点

- [registry.rs:226](../../crates/nyro-core/src/provider/registry.rs#L226)：在 `LEGACY_ORDER` 的 `nyro-gateway` 之后插入 `sub2api`；同步更新 `tests/fixtures/providers_legacy.json` 快照。
- [resolver.rs:138](../../crates/nyro-core/src/conversion/resolver.rs#L138)、[resolver.rs:163](../../crates/nyro-core/src/conversion/resolver.rs#L163)、[pipeline.rs:31](../../crates/nyro-core/src/provider/common/pipeline.rs#L31)、[pipeline.rs:51](../../crates/nyro-core/src/provider/common/pipeline.rs#L51)、[pipeline.rs:83](../../crates/nyro-core/src/provider/common/pipeline.rs#L83)、[pipeline.rs:419](../../crates/nyro-core/src/provider/common/pipeline.rs#L419)：改用按 provider 判断的 `is_codex_consumer` 和 `wire_dialect_vendor`；Fast 档函数改为接收 provider 推导出的标志，不再只接收 channel。
- `proxy/dispatcher/compat.rs`：flavor 选择与 service tier 覆盖；`proxy/dispatcher/mod.rs`：错误分类钩子、粘性 header（P2）。
- [providers.rs:282](../../crates/nyro-core/src/admin/providers.rs#L282)：模型探测 URL 改走 vendor 构造器；`test_provider` 增加 sub2api 分支；create / update 时规范化根地址、强制 Bearer，并校验 channel（拒绝 `typesafe` 和未知值）。
- [model_catalog.rs:69](../../crates/nyro-core/src/admin/model_catalog.rs#L69)、[model_catalog.rs:112](../../crates/nyro-core/src/admin/model_catalog.rs#L112)：sub2api 模型发现一律使用 Bearer。
- [usage.rs:370](../../crates/nyro-core/src/admin/usage.rs#L370)、[usage.rs:424](../../crates/nyro-core/src/admin/usage.rs#L424)：新增 `Sub2Api` 上游后端，身份优先识别，旧渠道的本地统计作为降级路径；`provider_api_price.rs` 为 `sub2api_key` 附带 `api_price`。
- WebUI：[providers.tsx](../../webui/src/pages/providers.tsx)（中继地址、可选协议、提示、过滤 deprecated、迁移提示）、[provider-usage.ts](../../webui/src/lib/provider-usage.ts)（身份优先、标题、价格支持）、[provider-speed-mode.ts](../../webui/src/lib/provider-speed-mode.ts)。

### 4.4 渠道同名风险审计（必须补上 vendor 判断）

sub2api 的 channel id 与已有渠道同名（`antigravity`、`openai`、`grok`……）。以下判断只看 channel，必须加上 vendor 条件：

- [providers.rs:158](../../crates/nyro-core/src/admin/providers.rs#L158)：Gemini 探测只按 channel 识别 antigravity / gemini-cli。如果不改，`sub2api/antigravity` 会被包成 v1internal 信封，并请求 `/v1internal:streamGenerateContent`，这是确定会出现的 bug。
- [providers.rs:448](../../crates/nyro-core/src/admin/providers.rs#L448)：`google_subscription_not_found_hint`。
- [google_display.rs:24](../../crates/nyro-core/src/quota_buckets/google_display.rs#L24)：`google_quota_channel`。
- `provider/google/antigravity.rs:493` 的 `is_antigravity_channel`，以及 `admin/usage.rs:2977`（当前都在 Google 分支内部，补上判断作为防御）。
- 新增回归测试：遍历每个 sub2api channel，断言 Google 订阅、Claude Code、Codex OAuth、xAI Grok、OpenCode Go、nyro-gateway、Devin / Kiro 的判定函数全部返回 false；同时断言主机名包含 `z.ai`、`api.kimi.com`、`token-plan.` 时，`UsageBackend::detect` 仍然识别为 sub2api。

### 4.5 调研中顺带发现的现有问题

1. [providers.rs:399](../../crates/nyro-core/src/admin/providers.rs#L399)：Gemini 探测曾在 `?alt=sse` 后再追加 `?key=`，得到 `...?alt=sse?key=...`；P1 已改为 `&key=`。
2. 用量后端的 URL 子串匹配（[usage.rs:533](../../crates/nyro-core/src/admin/usage.rs#L533) 的 `z.ai`，WebUI 中也有同样的判断）会命中任何包含 `z.ai` 的主机名（如 `xyz.ai`），建议改为匹配主机名后缀。

## 5. 测试计划

- **单元测试**：根地址规范化表；13 个平台 × 协议的 URL 矩阵；元数据不变式（共享 Key、`base_urls` 为空、所有协议都是 Bearer、可选协议是 `base_urls` 的子集、默认协议不在可选列表中、不包含 typesafe）；方言映射；Codex 契约与 effort 策略在 `openai/sub2api` 和 `sub2api/openai` 两种写法下参数化一致；§4.4 的同名回归；`/v1/usage` 各模式的样例（quota_limited 带窗口、订阅、钱包、过期、耗尽、`isValid=false`、404 降级）以及对应的调度结果；错误分类。
- **集成测试**（`crates/nyro-core/tests/sub2api_*.rs`）：用 axum 模拟 sub2api，记录方法、路径、header 和请求体。对每个平台 × 入口（chat / responses / messages / gemini）断言：路径与前缀正确、使用 Bearer、没有 `?key=`、直通或转换符合预期、openai 平台执行 Codex 清洗；403 `billing_error` 时故障转移到第二个后端；`/v1/usage` 映射结果与暂停调度。
- **WebUI**：中继地址规范化、预设协议初始状态（可选协议不勾选）、Fast 档显示条件、用量 kind 与标题；运行 `npm run lint` 和 `npm run build`。
- **人工 E2E**：对真实 sub2api 的每类平台分组各取一个 Key，验证模型发现、连接测试、模型探测，以及 Claude Code、Codex、Gemini CLI、OpenAI SDK 经 nyro 访问。
- **命令**：`cargo test -p nyro-core sub2api`、`cargo clippy -p nyro-core --all-targets`、`make check`。

## 6. 分期

| 阶段 | 内容 |
|---|---|
| **P1 可用** | vendor 与 13 个通道；URL 与鉴权；模型发现；方言映射与 Codex 契约一致；同名审计；探测 / 连接测试修正；WebUI 预设、地址、勾选器；旧渠道标记 deprecated；文档与测试 |
| **P2 运营** | `/v1/usage` 后端与调度；错误分类（计费错误故障转移）；平台识别；会话粘性；旧渠道迁移按钮；YAML `channel` 字段 |
| **P3 增强** | 计费倍率展示；Embeddings 端点（勾选器支持端点级 id）；签名域混用警告；count_tokens 直通（需要新入口）；上游 PR（`group_platform` 和窗口起点） |

## 7. 风险与待确认

1. 以上 sub2api 行为来自 `81ce8356d` 的源码，sub2api 迭代很快。平台表集中在一个模块维护；保存时拒绝未知 channel，已有数据按通用行为继续运行。
2. 「gemini 分组不支持 `/v1/responses`」来自源码阅读，需要在线上实例确认。
3. DeepSeek 的 Anthropic 历史回放归一化在经过 sub2api 时是否需要，需要联调确认。
4. composite 分组在 nyro 侧无法得知模型会解析到哪个平台，按透明中继处理。
5. Antigravity 的模型名以 sub2api 返回的 `/v1/models`（账号映射键）为准，nyro 不做档位改写。

## 附录：Standalone YAML 示例（P2 增加 `channel` 之后）

```yaml
providers:
  - name: sub2api-antigravity
    vendor: sub2api
    channel: antigravity
    default_protocol: anthropic
    endpoints:
      anthropic:
        base_url: https://sub2api.example.com
      gemini:
        base_url: https://sub2api.example.com
    api_key: ${SUB2API_ANTIGRAVITY_KEY}
    models_source: https://sub2api.example.com/v1/models
```

## 8. P1 实现记录

| 模块 | 内容 |
|---|---|
| 元数据 | `ChannelDef` 新增 `default_protocol`、`relay`（`RelayAddressing`：`models_path` / `route_prefixes` / `root_suffixes`）、`optional_protocols`、`deprecated`、`notes`，以及 `ChannelDef::DEFAULT`；nyro-gateway 改用 `relay` 声明 |
| vendor | [sub2api/mod.rs](../../crates/nyro-core/src/provider/sub2api/mod.rs)（13 个平台通道、`Sub2ApiVendor`、保存时规范化 `normalize_admin_fields`）、[platform.rs](../../crates/nyro-core/src/provider/sub2api/platform.rs)（`Sub2ApiPlatform`：路由前缀、方言、Codex 契约、透明中继、模型发现路径）、[url.rs](../../crates/nyro-core/src/provider/sub2api/url.rs)（根地址规范化与 URL 构造） |
| 方言 | [dialect.rs](../../crates/nyro-core/src/provider/dialect.rs)：`wire_dialect_vendor`、`is_codex_consumer`、`is_openai_codex_consumer`、`is_openai_native_responses`、`is_transparent_relay`；resolver、pipeline（Codex 三道消毒、Fast 档、effort 策略）、Codex 速度路由提示改用这些判定 |
| 管理面 | 保存时校验平台、规范化根地址、强制 Bearer、补全模型发现地址；模型发现一律 Bearer；连接测试走平台路由的模型列表；模型探测按平台前缀 + Bearer；探测渠道改为 vendor 限定（`probe_channel`），Sub2API Antigravity 不再被包成 v1internal 信封 |
| 用量 | Sub2API（新预设与旧渠道）按身份识别为本地统计，`UsageBackend::detect` 返回 `None` |
| 顺带修复 | Gemini 探测 URL `?alt=sse?key=` → `?alt=sse&key=` |
| WebUI | Sub2API 预设卡片与图标；中继地址组件由元数据驱动（[relay-address.ts](../../webui/src/lib/relay-address.ts)）；协议勾选器标注可选协议并预览最终路由；平台提示；通道默认协议；Fast 档只对 Codex 消费级通道显示；旧 `openai/sub2api` 渠道新建时隐藏 |
| 测试 | [sub2api/tests.rs](../../crates/nyro-core/src/provider/sub2api/tests.rs)（根地址、URL 矩阵、元数据不变式、平台分类、方言、同名回归）、[dialect.rs](../../crates/nyro-core/src/provider/dialect.rs) 单元测试、[sub2api_provider.rs](../../crates/nyro-core/tests/sub2api_provider.rs)（mock Sub2API 端到端：Antigravity 前缀、Gemini 转换与发现、OpenAI Codex 契约、多协议直通、平台不符提示、未知平台拒绝、模型探测路由）、WebUI [relay-address.test.ts](../../webui/src/lib/relay-address.test.ts) / [sub2api.test.ts](../../webui/src/lib/sub2api.test.ts) |

P1 之后的进展见 §9。

## 9. P2 实现记录（额度、拒绝处理、平台识别）

| 模块 | 内容 |
|---|---|
| 用量与调度 | [admin/usage.rs](../../crates/nyro-core/src/admin/usage.rs)：Sub2API 始终仅保留本地日志统计（`LocalUsageAccounting::Sub2Api`，kind `sub2api`），不轮询上游 `/v1/usage`，也不以上游余额/额度暂停调度（避免私有/SimpleMode 零余额实例被误判不可用） |
| 拒绝识别 | [errors.rs](../../crates/nyro-core/src/provider/sub2api/errors.rs)：`classify(status, body)` 按 Sub2API 的错误码与原文识别计费拒绝（`billing_error`、`insufficient_quota`、`INSUFFICIENT_BALANCE`、`API_KEY_QUOTA_EXHAUSTED`、`API_KEY_EXPIRED`、订阅/限额/RPM 码，以及 `rate_limit_exceeded` 和 Google 信封下只能靠原文的情形）和四类配置拒绝；上游账号自身的 429/限流不算 |
| 故障转移 | dispatcher 三条 HTTP 错误出口（非流式、流式、compat）给命中的响应打 `Sub2ApiRejection` + 健康中性；无论状态码（含 403/404）都切到下一个路由目标 |
| 提示 | 请求日志 `error_causes` 追加英文提示（`LogDiagnostic::add_cause`，不改失败结果/类型/阶段）；模型探测、连接测试的失败信息同样附带提示，中文界面显示相应的处理建议 |
| 平台识别 | [detect.rs](../../crates/nyro-core/src/provider/sub2api/detect.rs)：并行读取 `/v1/models`、`/v1beta/models`、`/v1/sub2api/billing?include=group`；纯函数 `infer` 给出平台、置信度（高/中/低）、候选和依据；分组名只用于打破平局。管理接口 `POST /api/v1/sub2api/detect-platform`、桌面命令 `detect_sub2api_platform` |
| WebUI | 「识别平台」按钮与结果面板（[sub2api-platform-detect.tsx](../../webui/src/components/sub2api-platform-detect.tsx)，依据中英文本地化，可一键改用建议平台）；新建，或编辑时平台/地址/Key 变化后保存，会复用或自动运行识别，与中/高置信度结果不一致时弹窗确认，识别失败不阻止保存；用量卡片仅保留本地 `sub2api` 视图 |
| 测试 | 单元测试覆盖拒绝分类、平台推断与健康中性标记；[sub2api_provider.rs](../../crates/nyro-core/tests/sub2api_provider.rs) 覆盖本地统计且不请求上游 `/v1/usage`、计费拒绝故障转移与提示（不暂停调度）、404 配置拒绝故障转移、探测提示、平台识别；WebUI 覆盖本地用量渲染与识别规则测试 |

仍未实现：§3.7 旧渠道「一键迁移」、§3.11 会话粘性、YAML `channel` 字段，以及 P3 各项。
