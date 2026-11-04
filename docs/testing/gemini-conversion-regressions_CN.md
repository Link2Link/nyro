# 原生 Gemini 转换修复与回归证据

[English（规范版本）](gemini-conversion-regressions.md)

## 当前状态

四项已批准的修复均已实现。测试阶段的基线是 `14a27aa`（`nyro-core` 2.0.9）：当时 21 个新用例中有 13 个断言失败、8 个对照通过。没有忽略或弱化这些测试；修复还补充了边界及本地真实 dispatcher 测试。

没有修改供应商配置、依赖、数据库、已部署服务或版本号，也没有提交代码。合成数据和本地 mock 上游，不等于已证明真实 Google/Vertex 账号接受相应请求。

## 已实现的行为

### A. 请求内的工具调用身份

`google/gemini/stream.rs` 现在维护 parser 实例内的调用计数器及显式 ID 状态：

- 独立完整调用获得不同 index；同名、参数相同但没有 ID 的调用也不会合并。Start 与参数 Delta 使用一致 index。
- SSE 传输分块不重置索引，不同请求不共享状态，不需要全局 map 清理。
- 保留上游非空 ID，缺失时才生成。
- 相同显式 ID、相同完整载荷的重复事件不重新输出调用或拼参数；同 ID 的名称/参数发生冲突时终止为流错误。
- 不按文本前缀、函数名或 part 位置猜累积语义。通用 partial-args/累积方言不在当前完整调用契约内。

这些改动修复了 Chat SSE index 冲突、强制流式聚合只剩最后一个调用、Responses custom 工具桥接串参。聚合器和各 formatter 保持原有索引契约。

### B. 显式空参数与安全错误结束

完整 `functionCall` 的 `args:{}` 现在会输出恰好一次 `"{}"` 参数 Delta，因此能保留到 Gemini 流式重编码结果中。缺失或非对象参数、非法函数名、损坏或截断 JSON 不会被补成可执行工具调用。

dispatcher 新增统一的批次校验，在工具缓存和 formatter 之前识别 decoder `Err` 以及 IR `StreamError`/`UnexpectedEof`：

- 原生流式输出协议对应的错误事件，不再成功完成，也不把挂起的 custom 参数补发为成功调用。
- 强制上游流式→客户端非流式路径返回 HTTP 502，而不是部分成功的工具响应。
- 记录失败；原始上游 HTTP 状态仍可能是 200。

更早批次已交付的文本/调用无法撤回。本次没有新增重试机制，也不声称支持任意第三方流格式。

### C. 按调用身份解析结果的函数名

`google/gemini/tool_names.rs` 在编码期间为文本结果和 ToolResult block 查找函数名，区分 IR ID 与上游函数名，覆盖 ToolUse blocks，并避免同一调用的重复表示造成冲突。不同函数的结果倒序返回也能精确关联，不靠 FIFO-only 猜测。

保留已有 call/result wire ID，包括原生 Google 输入中的 ID。旧式 Google 结果只有函数名时，按待处理同名调用的出现顺序配对；不透明 OpenAI/Anthropic ID 不采用该退路。名称使用 namespace/custom 工具准备后的上游名称。

无法关联的结果返回类型化 bad request。通用孤立结果修复会附带内部来源标记，避免把合成 `unknown_tool` 当成已找到真实函数；该标记不会泄漏到其他协议上游。旧测试中 `functionResponse.name == call_id` 的错误期望已改为真实函数名及配对 ID，孤立 fixture 另有明确策略测试。

### D. 有界的 Schema 语义转换

`google/gemini/schema.rs` 在原生 `parameters` 清理之前进行转换：

- 基于原始根节点解析局部 JSON Pointer `$ref`/旧式 `ref`，支持转义及 URI fragment。递归栈区分真实循环与兄弟字段复用。
- 合并兼容 object `allOf`，required 保持首次出现顺序；冲突或不能证明安全的 closed-object 交集拒绝，不后写覆盖。
- 不同字符串单值分支精确转为 enum。显式 JSON 类型可证明互斥的 oneOf 转为 anyOf，保留分支顺序、schema 和注释；integer 与 number 视为重叠，nullable 分支包含 null 域。重叠、缺少类型或无法证明安全的分支以及有损交集仍拒绝，不放宽语义。
- required 与 nullable 独立；属性名和 default/enum 字面量不会被误当 schema 关键字处理。
- 深度上限 64、访问/复制节点数上限 10,000（包含边界值）；测试覆盖边界和重复引用展开。不请求外部 ref。

无法表示的原生结构返回类型化 `ProtocolLossyRejected`（422），不再返回空 schema 的成功请求，也不会意外变成内部错误。pipeline 保留这类错误的状态，但不把其他 anyhow 错误无差别当成客户端错误。原生请求已显式选择的 `parametersJsonSchema` 保留，不未经验证地将所有端点自动切换到富通道。

### E. 调度工具联合类型与发送前失败日志

脱敏回归复现 `schedule_create`、`schedule_update` 的 `at.oneOf = [string, object{date,time,time_zone}]`，以 80 个工具分别通过原生直接转换和真实 Nyro→Nyro loopback 链路，覆盖流式与非流式。两个分支和本地日期/时间的 required 成员必须保留，不能换成无约束 schema。原有 native Schema-proto 关键字清理不变，包括移除 `additionalProperties`；这不等于完整 JSON Schema 校验等价或真实 Antigravity 验收。

`passthrough_run`、`build_request` 返回的失败记为本地请求构建错误，不是上游 HTTP 错误。本轮不改变其他发送前准入、钩子或运行时准备分支。构建失败日志保留关联的客户端请求 ID、所选路由/供应商/模型、类型化客户端状态和 `request_build` 阶段，不伪造上游状态或已发送请求。每个失败有独立 attempt 编号；fallback 成功仍是客户端最终结果。本地准备失败不消耗远端尝试预算，也不惩罚供应商健康状态。诊断原因有长度与凭证安全限制，载荷沿用 collector 的异常强制保留规则。

## 原生与 compat 校验边界

dispatcher 只有在选定 raw-wire compat 后，才设置客户端无法反序列化的私有 `RequestMetadata.raw_wire_preview`。该路径的原生 before/after 编码仅用于 vendor diff，不是最终请求；其 schema/孤立结果校验不能提前否决 compat 可正确表示的请求。最终 wire 仍由选定 compat 内核构造和校验。

影响仍取决于具体路径：

- Google `antigravity`/`gemini-cli` 为执行 envelope hooks 和强制流式处理，明确走原生 IR。
- Chat/Responses 客户端到 Gemini 出口使用原生转换。
- 普通 Anthropic→直接 Gemini 通常走 cc-switch，其富 schema 和名称行为保持不变。
- 无修改的同协议直通保留原始字段。
- Vertex 同时有原生 Gemini 与 OpenAI 兼容通道；依据协商端点判断，而不是供应商品牌。

## 验证结果

以下数量记录最初四项修复的基线验证，不是新增调度工具联合类型与构建失败日志回归后的数量。

该次仓库门禁完成，**没有失败的测试目标**：

- 原生流式回归：**21 个通过**。
- 原生请求回归：**16 个通过**。
- Loopback dispatcher 路由回归：**6 个通过**。
- `nyro-core` 库单测：**725 个通过**，包含真实聚合器、Schema 边界、预览校验归属和流错误传播测试。
- 完整 workspace 门禁（`--exclude nyro-desktop --no-default-features --no-fail-fast`）：**通过**；原有 server/doc-test 的忽略项未改动。
- `cargo clippy -p nyro-core --all-targets`：**通过，仍有仓库既存告警**，未使用 `-D warnings`。
- `cargo fmt --all -- --check`、`git diff --check`：**通过**。

### 调度工具联合类型与构建失败日志的追加验证

在基于 v2.1.25 的本地工作区、Rust 1.96.0 下验证：

- `cargo test -p nyro-core`：**2,280 个通过**，零失败；含 1,533 个库单测和 747 个集成测试。原有 6 个 doctest 忽略项未改动。
- 请求 codec 回归 **18 个通过**、路由回归 **10 个通过**、构建失败日志回归 **4 个通过**，均已包含在完整测试中。
- `cargo check -p nyro-core --all-targets`、`cargo fmt --all -- --check`、`git diff --check`：**通过**。
- `cargo clippy -p nyro-core --lib --test conv_google_request_regressions --test gemini_route_regressions --test request_build_logging`：**通过**，保留原有库告警。
- 完整 `cargo clippy -p nyro-core --all-targets` 仍被原有 Kiro runtime 测试的 `clippy::unused_io_amount` 阻挡（`runtime.rs:1606`，由 `95980b0e` 引入）；没有修改该无关测试或压制 lint。
- `cargo build --release -p nyro-server --no-default-features`：**通过**。此项验证不内嵌 WebUI 的服务器构建，不等于正式发版、部署或真实 Google 验收。

## 测试组织和命令

| 位置 | 作用 |
| --- | --- |
| `crates/nyro-core/tests/conv_google_stream_regressions.rs` | 调用身份、实例隔离、wire 组装、custom 桥接、零参数、非法输入、显式 ID、传输切分 |
| `crates/nyro-core/tests/conv_google_request_regressions.rs` | 倒序结果名称、ID 往返、旧式配对、孤立拒绝、引用/组合、nullable/required 和其他协议对照 |
| `crates/nyro-core/src/proxy/dispatcher/accumulator.rs` 内部测试 | 真实私有聚合器，不复制实现或暴露私有 API |
| `crates/nyro-core/src/protocol/codec/google/gemini/schema.rs` 单测 | 局部引用、转义、循环、精确 enum/交集规则、资源限制 |
| `provider/common/pipeline.rs` 单测 | 类型化错误状态及 compat 拥有的预览校验 |
| `proxy/dispatcher/streaming.rs` / `non_stream.rs` 单测 | 失败批次整体拒绝及强制流式 502 |
| `crates/nyro-core/tests/gemini_route_regressions.rs` | 真实 dispatcher + loopback 上游：原生、compat、直通、Vertex、schema 拒绝、失败流，以及 80 工具调度 schema 的直接/两跳路由 |
| `crates/nyro-core/tests/request_build_logging.rs` | 原生/直通构建失败、SQLite collector 持久化及公开 Admin 查询、最终结果关联、载荷隐私、fallback 与预算隔离 |

专项执行：

```bash
cargo test -p nyro-core --test conv_google_stream_regressions
cargo test -p nyro-core --test conv_google_request_regressions
cargo test -p nyro-core --test gemini_route_regressions
cargo test -p nyro-core --test request_build_logging
cargo test -p nyro-core --lib proxy::dispatcher::accumulator::tests
cargo test -p nyro-core --lib proxy::dispatcher::streaming::tests
cargo test -p nyro-core --lib proxy::dispatcher::non_stream::tests
cargo test -p nyro-core --lib protocol::codec::google::gemini::
```

既有转换及仓库门禁：

```bash
cargo test -p nyro-core --test conv_google --test conv_cross_provider \
  --test conv_streaming --test conv_tool_pairing --test protocol_conversion
cargo test --workspace --exclude nyro-desktop --no-default-features --no-fail-fast
cargo clippy -p nyro-core --all-targets
git diff --check
```

本地路由测试明确阻止 Gateway 启动时的 refresh/OAuth 监控任务被调度执行，使用内存存储、无效占位 token、禁用代理/跳转、仅访问 loopback HTTP。订阅认证绑定会锁定真实服务地址，所以没有伪装成真实订阅凭证访问上游；其强制流式路径由 pipeline/handler 测试覆盖。

## 明确没有修改的行为

- 不做全局相邻角色合并，不声称连续 user 已被证实导致空 HTTP 200。
- 不猜通用累积快照，不全局改造 OpenAI missing-index。
- 不把 nullable 当 optional，不声称 OpenAI strict 禁止局部引用。
- 不改配额冷却、图片映射、模型目录、重试，或其他无关签名/响应 metadata 设计。
- 不声称已做真实上游验收或部署；账号/模型 smoke 是另一项需授权的操作。
