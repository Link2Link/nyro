# 实验性原生 Cursor AgentService 提供方

[English](<cursor-provider.md>)

## 状态与范围

Nyro 的 Cursor 集成是**原生 Rust、仅用于出站的 AgentService 适配器**。
它不启动 Cursor CLI、Node/Bun、第二个网关、本地 shell 或工作区工具，
也不把 Cursor 伪装成 OpenAI 兼容上游。

当前实现处于实验阶段。开发期间离线编译与协议测试已通过，但**没有使用真实
Cursor 凭据，没有执行真实登录、模型发现、模型生成、工具往返或订阅计费验证**。
公开源码核对不代表生产可用性；固定的参考版本可能与特定账户正在使用的服务版本不同。

可选的丰富模型目录元数据与账户特定推理参数路由已实现，并由离线 fixture 验证：
仅匹配精确且已授权的 legacy ID，推理档位切换保持非推理成本/上下文参数不变。
这些能力仍未经真实账号上游验证。

## 架构

```text
OpenAI / Anthropic / Gemini 兼容客户端请求
    -> 现有入站解码器 -> AiRequest
    -> 出站协议协商：cursor-agent/run/v1
    -> Cursor Vendor::start_ir
       -> 凭据 + 账户可用模型发现
       -> 请求/历史能力检查
       -> 纯 CursorSession
          <-> 原生 HTTP/2 双工传输
          <-> api2.cursor.sh/agent.v1.AgentService/Run
       -> AiStreamDelta
    -> 现有累积器 / 客户端协议响应编码器
    -> 缓冲 JSON 或客户端流式响应
```

注册表声明的是原生出站协议，不暴露 Cursor HTTP 入站路由，也不注册假装能够
处理 AgentService 的 JSON codec。旧 JSON `build_request` / `parse_response`
路径明确拒绝使用。传输层和提供方策略保持分离：

| 边界 | 职责 |
|---|---|
| [协议消息](<../../crates/nyro-core/src/provider/cursor/wire.rs>) | 对照已核实字段手写最小 `prost` 消息，无 `protoc` 构建依赖 |
| [Connect 分帧](<../../crates/nyro-core/src/provider/cursor/framing.rs>) | 增量 envelope、gzip、trailer 错误和硬上限 |
| [历史](<../../crates/nyro-core/src/provider/cursor/history.rs>) | 调用方 IR 重建、SHA-256 内存 blob、system 规则、MCP schema/Value |
| [会话](<../../crates/nyro-core/src/provider/cursor/session.rs>) | 纯状态机：出站帧、IR 事件、可验证终止或明确错误 |
| [模型目录](<../../crates/nyro-core/src/provider/cursor/catalog.rs>) | 账户权威可用 ID 与模型路由数据 |
| [运行时](<../../crates/nyro-core/src/provider/cursor/runtime.rs>) | 认证头、发现、有界队列、心跳、取消与资源所有权 |
| [双工传输](<../../crates/nyro-core/src/proxy/client/duplex.rs>) | 强制 HTTP/2、上传流、响应数据及 trailer、deadline |
| [原生 IR 响应路径](<../../crates/nyro-core/src/proxy/dispatcher/ir_response.rs>) | 响应格式转换、终止/错误记账与能力限制头 |
| [OAuth 驱动](<../../crates/nyro-core/src/auth/drivers/cursor.rs>) | 原生 PKCE/轮询/刷新，不抓取浏览器凭据 |

## 服务与模型目录契约

- 固定服务源：`https://api2.cursor.sh`，不是可配置的第三方中转地址。
- 生成：`/agent.v1.AgentService/Run`，HTTP/2 双工，
  `application/connect+proto`。
- 基础模型发现：`/agent.v1.AgentService/GetUsableModels`，
  `application/proto`，**裸 protobuf body**，不带 Connect 五字节帧头。
- 模型发现与账户/凭据绑定。运行时按提供方标识、凭据指纹与客户端版本短期缓存
  已解码目录；不缓存活会话，也不把 bearer 明文作为 map 键。
- 模型 ID 原样保留。基础实现不推断 family alias、effort 后缀改写或替代模型。
- 路由可表达独立的 legacy `modelDetails.modelId`、
  `requestedModel.modelId`、参数列表及 max-mode。丰富路由必须来自权威服务端元数据，
  不能依赖名称猜测。
- 客户端身份采用固定参考版本 `cli-2026.09.02-c22c1a3`，这是协议头，
  不代表安装或调用该 CLI。

管理员的提供方探测走同一条原生 Run 路径。执行探测属于**真实上游调用**，可能消耗
订阅额度；本次离线实现验证没有执行该操作。

## 请求与历史语义

每个客户端请求都建立全新会话和 conversation ID，根据请求携带的完整历史重建状态。
不存在跨请求活 HTTP 会话、隐藏 checkpoint resume 缓存、本地工具执行器或后台 agent 循环。

历史提示采用 Cursor 内部消息形状的 JSON blob，通过 SHA-256 ID 填入
`rootPromptMessagesJson`。类型化展示 turns 通过同一内存存储引用 user 消息和
assistant/thinking/MCP steps。工具调用保留 ID、名称与 JSON 参数；配对结果保留空内容
和明确错误。孤立结果、重复 ID 和未回答调用明确拒绝，而不是转换成虚构的 assistant 文本。

末尾用户消息通过 `userMessageAction` 发送，不重复放入历史 prompt/turn，包括显式空用户
消息。末尾工具结果使用 `resumeAction`，携带**完整重建历史**，不是恢复缓存中的活 Run。
不生成 XML 工具结果包装，也不伪造 assistant 确认。

system 文本是调用方配置，同时写入历史 system 项和 `RequestContext` 的 global USER
规则。规则路径只是协议标签，不会读取或写入宿主文件。真实系统提示仍由 Cursor 控制，
调用方配置仅能**尽力保留**；不宣称支持需要服务端 allowlist 的 `custom_system_prompt` 覆盖。

## 工具与本地交接

只把调用方声明的工具广告为 MCP 定义。schema 字节和参数 map 的每个值均为序列化
`google.protobuf.Value`，不是 JSON 字节或裸 `Struct`。null、布尔、数组、嵌套对象和空对象
保留原含义。损坏 Value、缺失 kind、非有限数、无法精确保留的大整数以及非对象参数 JSON
都会明确失败。

即使名称为 `read`、`bash` 等，也不会因类似原生工具而被静默删除或改名：它们仍是调用方
拥有的 MCP 工具。`none` 不广告工具；named 只广告指定的已声明工具。required/named 未发生
交接时，不能悄悄作为普通文本成功终止。本适配器**不宣称完整 JSON Schema 校验**。

第一个已核实工具调用产生以下 IR：

```text
ToolCallStart -> ToolCallDelta（权威参数） -> ToolCallComplete
             -> Done(tool_use) -> Terminal(ToolHandoff)
```

参数预览先缓冲，等待最终类型化参数，因此被替换或未闭合的预览 JSON 不会作为完整调用
传给客户端。运行时随即关闭上游 Run。`ToolHandoff` 是刻意的**本地**终止，并不证明 Cursor
发送过 `turnEnded`。调用方执行工具后，用包含完整更新历史的新 HTTP 请求继续。
适配器不执行工具、不保留 Run 等待结果，也不发起并行工具执行。

已知原生 shell/文件系统操作获得类型化拒绝或错误响应；其他已识别原生 exec 家族获得
`ExecClientThrow` 和 stream-close，随后产生明确 unsupported 失败。未知操作、尚未实现
的交互审批、subagent 工作流和原生流式操作，不会被静默确认或当作已完成工作。

## 终止、错误与资源上限

普通成功必须同时满足：

1. 收到权威 `turnEnded` 更新。
2. 收到正常 Connect 结束 envelope，或明确成功的传输 status trailer。
3. 分帧完整，无未完成工具调用，也没有不支持的原生操作。
4. **先检查 trailer，再到达 HTTP 响应 EOF**。

文本输出、HTTP 200、`stepCompleted` 或单独 EOF 都不代表成功。较晚到达的错误 trailer
优先于此前 `turnEnded`。截断、损坏 protobuf、gzip 错误、未知操作、缺失 blob、SHA-256
不匹配均为类型化错误。

| 边界 | 基础限制 |
|---|---:|
| Connect 压缩 payload | 4 MiB |
| 解压后或未压缩 Connect payload | 8 MiB |
| 每会话 blob 数据总量 | 32 MiB |
| 单个 blob | 8 MiB 减去 envelope 预留空间 |

解码器处理分裂头/体和同一 HTTP data chunk 中的多帧，不信任尚未检查的长度。
传输保留 trailer，不把响应压成只有字节的流。上传和事件队列有界；取消、客户端断开或
deadline 到期必须释放会话资源。已提交 Run 不能仅因响应失败就被自动重放。

## 能力限制与用量

原生响应路径提供：

```http
x-nyro-provider-limitations: system_best_effort,token_budget_not_enforced,usage_unknown
```

- 接受正数 `max_tokens`，兼容强制要求此字段的客户端；但原生 Run 映射**不执行该预算**。
  零值无效。
- 支持纯文本响应格式；结构化 JSON/schema 输出与尚未核实映射的采样参数明确拒绝，
  而不是静默忽略。
- 基础历史只支持 text/thinking/function-call/result。多模态、server-tool block、
  custom/namespaced 工具与交互式 agent 操作明确不支持。
- 显式 reasoning 请求必须有权威目录路由。缺少丰富元数据不是按模型名称猜测路由的许可。
  客户端思考预算（`budget_tokens` / effort `budget`）没有 Cursor 线上等价物：按网关统一的
  阈值表换算为定性档位（与 Claude 5.5 中转改写共用：`>10240→max、>8192→xhigh、
  >1024→high、>0→medium`；预算换算档位优先于同时提供的定性 effort）。换算后的档位
  仍必须由权威 lane 元数据声明，否则请求按失败关闭处理。reasoning display/summary
  模式仍然拒绝。
- 只有权威 `TurnEnded` 计数用于 IR usage。会话中缺失计数仍为 unknown。
  `tokenDelta` 估计值与 checkpoint 的上下文占用不作为 completion token 估计。
  旧整数 IR 能表达完整 input/output 对，但不能表达所有缺失项；没有 Usage 事件不能
  被宣传为“实测用量为零”。

## 验证与后续

离线测试覆盖 split/sticky/gzip/上限/错误、protobuf golden 字节、精确模型 ID、内存 KV
完整性、历史重建、空/错误工具结果、context 握手、text/thinking、用量、终止时序、部分
参数、已声明工具检查与原生拒绝。集成还包含协议协商、OAuth 与 dispatcher 测试。
合入各并行修改后，应统一运行：

```bash
cargo test -p nyro-core --lib cursor
cargo check -p nyro-core
```

这些测试不能代替经过账户授权的真实验证。扩大使用范围前，应使用测试账户核实
OAuth/刷新、模型发现、文本流、调用方工具交接与继续、代理/HTTP2、取消、trailer 和实际计费。
丰富 AvailableModels 元数据、多模态和原生 agent 操作都是单独能力，不能作为基础支持推断。

## 协议来源

权威实现参考是 MIT 许可的 **oh-my-pi** commit
[`5d0aaffaf1f987503ce129a642fb6e30339615af`](<https://github.com/can1357/oh-my-pi/tree/5d0aaffaf1f987503ce129a642fb6e30339615af>)。
固定 schema、公开源码链接、适配说明与完整 MIT notice 见
[协议来源](<../../crates/nyro-core/src/provider/cursor/PROTOCOL_SOURCES.md>)。
没有使用 AGPL Astro 代码。
