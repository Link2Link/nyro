# cc-switch 协议转换能力移植报告

> 移植完成日期:2026-08-15
> 源项目:[cc-switch](https://github.com/farion1231/cc-switch) @ commit `eb69e4922ee187a261fd29c216a738e838f85bc4`(MIT)
> 目标位置:`crates/nyro-ccswitch-compat` + `crates/nyro-core/src/proxy/dispatcher/compat.rs`

---

## 1. 背景与目标

cc-switch 内置的本地路由(proxy)在多种 AI 协议之间做线级转换:Claude Code(Anthropic Messages)、Codex(OpenAI Responses)、Gemini CLI(Google GenAI)发出的请求可以被转换成任意一种上游协议,响应(含 SSE 流)再转换回客户端期望的形态。这套转换历经大量真实网关的兼容性打磨(推理加密回放、工具调用多轮、提示缓存、压缩炸弹防护等),行为细节非常密集。

本次移植的目标:**让 nyro 获得与 cc-switch 完全等同的协议转换线级行为**——不是"语义近似",而是可审计的断言级对齐;同时**保留 nyro 除协议转换之外的一切**(IR 编解码、dispatcher 管线、admin、存储、插件、故障转移等)不做替换。

## 2. 源项目分析结论

cc-switch 的转换层位于 `src-tauri/src/proxy/`,约 59K 行(含测试),其中协议转换核心约 25K 行。关键特征:

- **成对转换而非中间表示**:每个方向一对 `transform_*.rs`(请求/非流式响应)+ `streaming_*.rs`(SSE 状态机),以 `serde_json::Value` 为载体,按白名单重建字段(未知字段默认丢弃,避免严格网关 422)。
- **六个转换方向** + 一个同协议变形(见 §5 选路矩阵)。
- **状态化桥接**:Codex→Chat 的工具历史映射(`codex_chat_history`)、Gemini 的 thought_signature 影子存储(`gemini_shadow`)、reasoning 跨协议加密回放(`reasoning_bridge`,`ccswitch-*-v1:` base64url 前缀)。
- **测试形态**:1044 个内嵌 `#[cfg(test)]` 单测,纯函数断言 + SSE 流拼接,无 golden 文件 IO,天然可随代码整体移植。
- **可剥离性极好**:转换核心只依赖 serde/bytes/futures/tokio 等基础库,不触碰 Tauri/DB/配置。

## 3. 移植策略

| 决策 | 内容 | 理由 |
|---|---|---|
| 独立 crate | `nyro-ccswitch-compat`,机械移植的 `src/ported/` 保持源文件名与结构 | 逐行可对照、可随上游更新再同步;不污染 nyro-core 的 IR 语义 |
| 字节级门面 | `CompatEngine` 只收发 `Bytes`/简单类型,内部 `serde_json::Value`(vendored preserve_order 版本)不外泄 | 阻止有序 JSON 语义经 feature 合并渗入 nyro-core 的 serde_json |
| 选路集中在 dispatcher | `proxy/dispatcher/compat.rs` 的 `supports/select_compat_request` 决定何时走兼容层,其余路径(IR、Native 透传)原样保留 | cc-switch 的 Provider 配置判定被映射到 nyro 的协议协商(egress ProtocolId)上,单一权威来源 |
| 断言级对齐审计 | `scripts/check_cc_switch_parity_inventory.py` + `tests/parity_inventory.toml` 对 1168 个源测试逐一分类锁定 | "完全等同"必须可证明:migrated(逐行移植)/ mapped(断言等价)/ not-applicable(policy 批准的排除) |

## 4. 模块映射清单

### 4.1 直接移植(`ported/`,状态 migrated,511 个测试)

| cc-switch 源文件 | nyro 位置 | 职责 |
|---|---|---|
| `providers/transform.rs` | `ported/providers/transform.rs` | Anthropic ↔ Chat 请求/响应转换 |
| `providers/transform_responses.rs` | 同名 | Anthropic ↔ Responses |
| `providers/transform_codex_anthropic.rs` | 同名 | Responses ↔ Anthropic(Codex→Claude 网关) |
| `providers/transform_codex_chat.rs` | 同名 | Responses ↔ Chat(4558 行,最大单件) |
| `providers/transform_gemini.rs` | 同名 | Anthropic ↔ Gemini |
| `providers/transform_codex_responses_namespace.rs` | 同名 | Codex 私有 namespace 工具扁平化/还原 |
| `providers/transform_codex_responses_xai_sanitize.rs` | 同名 | xAI 严格 Responses 字段清洗 |
| `providers/streaming*.rs`(5 个) | 同名 | 各方向 SSE 状态机(工具增量聚合、usage、finish_reason 映射) |
| `providers/{reasoning_bridge,codex_responses_sse,codex_chat_common,codex_chat_history,gemini_schema,gemini_shadow}.rs` | 同名 | reasoning 加密桥、共用 SSE 构造器、有状态桥 |
| `sse.rs` / `json_canonical.rs` / `tool_media.rs` / `content_encoding.rs` / `handlers_compat.rs`(聚合兜底) | `ported/` 对应位置 | SSE 解析(UTF-8 跨 chunk)、canonical JSON、工具媒体搬运、有界解压 |

### 4.2 门面与集成(nyro 侧新增)

| 模块 | 职责 |
|---|---|
| `engine.rs`(CompatEngine) | 六方向 + 归一化直通的 prepare/convert;流启动检查、语义失败检测、SSE 聚合、Codex 客户端错误封套 |
| `profile.rs` | `ConversionProfile`/`Direction`/`UpstreamFlavor`、Chat reasoning 配置推断 |
| `session.rs` | 会话身份提取(Anthropic 头/metadata.user_id、Codex/GrokBuild 头)与转换会话 |
| `transport.rs` | Header/BodyKind/`should_force_identity_encoding`/诊断分类 |
| `state.rs` | codex_chat_history + gemini_shadow 的进程内状态 |
| `nyro-core:dispatcher/compat.rs`(重构基线约 3200 行，含测试) | 选路矩阵、首块 priming/重放、头归一化(Codex 指纹剥离、1M 上下文 beta)、usage 记账、错误封套接线 |

### 4.3 判定层映射(mapped,149 个测试)

cc-switch 的 `providers/claude.rs`/`codex.rs`/`forwarder.rs`/`handlers.rs`/`response_processor.rs` 中与协议行为相关的判定,映射到 nyro 的对应测试(引擎级或 dispatcher 级),每条在清单中记录断言级说明。典型例:

- `needs_transform` 矩阵 → `needs_transform_matrix_matches_egress_protocol`(cc-switch 的 meta/settings/TOML 三层配置优先级收敛为 nyro 的单一协商协议);
- prompt_cache_key 显式>会话优先级、`stream_options.include_usage` 注入、reasoning_content 厂商判定 → `engine.rs` 的 `claude_transform_*` 系列;
- 413 指向上游/转发失败含上下文/非标准体归一化 → `dispatcher/compat.rs` 的 `codex_proxy_*` 端到端测试。

### 4.4 排除(not-applicable,508 个测试)

按 policy 分 12 类批准排除:`excluded-auth`(凭证/OAuth 归 nyro provider 适配器)、`excluded-failover`(熔断/故障转移归 nyro dispatcher)、`excluded-config`、`excluded-routing`(URL 改写归 nyro 协商)、`excluded-observability`、`excluded-optimizer`、`excluded-usage` 等——均属"nyro 已有等价所有物"或"非协议转换闭包"的部分,每条带固定理由文本。

## 5. 集成:选路矩阵

dispatcher 在 `supports_compat_request` 中按 `(ingress, egress, vendor)` 选择兼容层,命中则**绕过 IR 往返**,直接以原始字节走 cc-switch 语义:

| ingress → egress | 方向 | 典型场景 |
|---|---|---|
| Anthropic → OpenAI Chat | `AnthropicToChat` | Claude Code 接 DeepSeek/Kimi/OpenRouter 等 |
| Anthropic → OpenAI Responses | `AnthropicToResponses`(CodexOAuth/xAI/标准三口味) | Claude Code 接 ChatGPT Codex 反代、xAI |
| Anthropic → Gemini | `AnthropicToGemini` | Claude Code 接 Gemini 系 |
| Responses → OpenAI Chat | `CodexResponsesToChat` | Codex 接 Chat 系(Kimi/DeepSeek) |
| Responses → Anthropic | `CodexResponsesToAnthropic` | Codex 接 Claude 系网关(`[1m]` 后缀、1M beta 头) |
| Responses → Responses(xai) | `XaiResponsesNative` | xAI 原生 Responses 的 namespace 扁平化 + sanitize |
| Anthropic → Anthropic(仅 DeepSeek/MiMo 系) | `AnthropicToAnthropic` | 同协议直通 + thinking 历史回放归一化 + DeepSeek 官方 effort 剥离 |

未命中组合照旧走 nyro 原有 IR 转换或 Native 透传——**原有能力零替换**。

## 6. 本轮补齐的行为(相对移植开始时的在制状态)

1. **`ported/providers/claude_compat.rs`**(21 个测试):DeepSeek/MiMo 工具历史 thinking 回放(缺失注入 `"tool call"` 占位、签名剥离、redacted 重写为 `"[redacted thinking]"`);DeepSeek 官方端点 `thinking:disabled` 与 effort 互斥剥离(尾斜杠 URL 归一化);Claude Code 身份注入(幂等,`system` 字符串→数组)。Kimi 按其 2026-08 反馈刻意排除。
2. **`Direction::AnthropicToAnthropic`**:归一化直通方向,响应原样回传(usage 照记)。
3. **缓存键语义**:Responses 路径显式>会话优先级;Chat 路径仅显式注入(与会话键隔离)。
4. **`should_force_identity_encoding`**:仅流式(请求体 stream、Gemini SSE 端点、`Accept: text/event-stream`)强制 identity;非流式恢复自动压缩(此前一律强制 identity,与源不符)。
5. **`codex_client_error_json`**:413 的"指向上游 + /compact 指引、不回显 nginx HTML"、转发失败的本地上下文、MiniMax `base_resp` 等非标准体归一化(结构化 code/upstream_status 字段)。
6. **无效客户端历史快速失败**:`CompatError::is_invalid_request()` → 400 立即返回,对齐 cc-switch 的 NonRetryable 分类。
7. **响应解析错误字段诊断**:content-type/encoding/body-bytes/body-kind,不含正文。
8. 修复在制遗留:xai vendor 新增 openai-responses 端点后未同步的元数据快照;两个断言方向写反的集成测试(未知字段保留、delta 合并——源语义是白名单重建、不合并)。

## 7. 对齐审计机制

```bash
python3 scripts/check_cc_switch_parity_inventory.py \
  --source /home/ubuntu/code/cc-switch \
  --commit eb69e4922ee187a261fd29c216a738e838f85bc4 \
  --require-complete
```

- 源测试清单**永远从 git commit 读取**,不信任工作树;每个测试项(从 `#[test]` 属性到闭括号)计算 SHA-256。
- `migrated` 仅允许直接移植文件(路径锁定);`mapped` 需 target 定位符 + target SHA-256 锁定 + ≥30 字符且不含 PENDING 字样的断言级说明;`not-applicable` 需 policy 批准的排除分类与固定理由。
- 目标测试扫描域:compat crate、`nyro-core/src/proxy/dispatcher/compat.rs`、`nyro-core/tests/cc_switch_parity.rs`。
- 代码重排(如 fmt)后需带 `--update-targets` 重跑刷新哈希。
- 2026-08-22 重构基线复核结果：**Integrity: OK, Completeness: COMPLETE**——1168 = 511 migrated + 149 mapped + 508 not-applicable，发现目标测试 699 个，pending 0。

## 8. 测试与验证

| 门禁 | 结果 |
|---|---|
| `make test`（移植完成时历史门禁） | 23 个测试二进制，**1307 通过 / 0 失败**（vendored serde_json 的上游 doctest 已关闭） |
| `cargo test -p nyro-ccswitch-compat --lib` | **663 通过 / 0 失败**（2026-08-22 重构基线复核，含直接移植测试与 Nyro 外层扩展测试） |
| `cargo test -p nyro-core` | **全部 lib / integration / doctest 通过**（2026-08-22 Stage 6；401 个 lib 测试，PassThrough fidelity 23 个，dispatcher compat 36 个） |
| `cargo check --workspace --exclude nyro-desktop` | 通过 |
| `cargo fmt --all -- --check` | 干净 |
| `cargo clippy`（`nyro-core` + `nyro-ccswitch-compat`, all-targets） | **0 error**；Stage 0 修改文件无新增告警，保留 nyro-core 既有 8 个 lib + 1 个测试告警 |
| 对齐审计 `--require-complete` | exit 0,COMPLETE |

dispatcher 级测试通过真实 `reqwest` 客户端 + 本地 TCP mock 上游驱动 `handle_compat` 全链路(选路→转换→首块 priming→流式转换→usage 记账→错误封套),不是纯函数抽查。独立验证 agent 复核全部门禁并做过对抗探测(伪造 commit、篡改清单均被审计拦截),结论 PASS。

## 9. 已知边界与有意差异

1. **Provider 配置面**:nyro 的 Provider 是规范化模型(vendor/protocol/base_url/channel),没有 cc-switch 的 settings/meta JSON 配置面。因此 `prompt_cache_key` 用户覆盖开关、Codex OAuth 快速模式开关等只保留**引擎层能力**(选择层无输入源),已在对应映射说明中记录。
2. **错误封套产品名**:`"Nyro local proxy failed"` / `nyro_*` 错误码,替代源串的 CC Switch 命名;结构、字段与行为保持一致。
3. **机械移植的代码风格**:`ported/` 保持与源逐行可比的展开形态(模块级 `#![allow(clippy::collapsible_if, dead_code)]` 并注释缘由);部分为完整性而移植的助手暂未被门面引用。
4. **smoke 脚本**:Makefile 的 `smoke` 目标引用的 `scripts/smoke/server_smoke.py` 在本检出中不存在(仓库既有状态),E2E 覆盖由上述 dispatcher 级真实链路测试承担。

## 10. 许可与归属

- 源许可逐字节复制于 `THIRD_PARTY_LICENSES/cc-switch-MIT.txt`(仓库根,审计契约指定),与源 commit 的 LICENSE 经审计比对一致。
- `ported/` 各文件头与 `lib.rs` 均标注来源 commit 与版权;crate `license = "Apache-2.0 AND MIT"`。

## 11. 维护指南

- **修改转换行为**:先改 `ported/` 对应文件并与 cc-switch 同名文件对照,再跑 `cargo test -p nyro-ccswitch-compat`;任何与源有意分叉处必须在文件头注释说明。
- **同步 cc-switch 更新**:锁定新 commit → 逐文件 diff 移植 → `--initialize` 重建清单(或逐条 `--update-target`)→ 重审 mapping 文本 → `--require-complete`。
- **新增目标测试**:只能放在扫描域内(见 §7);之后带 `--update-targets` 重跑审计。
- **勿动**:`ported/` 的白名单重建语义(未知字段丢弃)是源行为,不是缺陷;nyro 的 IR 路径继续服务未命中选路矩阵的组合。
