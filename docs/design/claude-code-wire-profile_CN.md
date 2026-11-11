# Claude Code 生成请求的协议配置

[English](<claude-code-wire-profile.md>)

## 范围

Anthropic 的 `claude-code` 通道在 `egress::prepare` 处理非原生客户端，位置是 dispatcher 完成供应商/客户端 Header 合并和协议请求体构建之后。覆盖 Messages、流式 Messages 及通道自身的 token 计数路径；普通 Anthropic API-key 供应商不进入此适配器。管理侧模型探测复用同一入口。

这些机制用于协议兼容，**不保证账号安全、订阅适用性或缓存命中**。验证使用本地 fixture，不代表上游允许原本不支持的使用方式。

## 一致的生成请求配置

生成请求使用一组版本化配置：CLI **2.1.321**、Stainless **0.128.0**、runtime **v26.12.1**，以及一致的 billing 版本和按请求推导的 beta 规则。默认值和能力顺序参考 CLIProxyAPI `b4e7dbd2`，不再是此前仅提升 UA 版本的混合配置。

非原生客户端不能通过入站或已合并的 Header 单独覆盖生成身份字段：适配器整体重建并校验最终版本组合及 billing。没有新增逐字段 profile 配置，也不会从客户端 Header 学习版本。已确认原生客户端保留现有独立的 2.1.x、patch >= 280 兼容范围；其软件身份不经过生成请求的严格版本校验。

Beta 根据处理后的请求体、模型、凭据和请求类别推导。配额探测不等于普通 Messages；thread 对象关联 thread beta；一小时 Messages 缓存标记必须匹配 extended-cache beta。Token 计数保持自己的 beta 列表，不套用 Messages 缓存策略。未知 beta 的转发延续现有网关策略。

## 缓存规范化

非原生 Messages 的自动模式依次处理：

1. 将有效顶层 `cache_control` 放到最后合格内容块；必要时退到 system 或非 deferred tool。找到目标后才删除顶层。目标已有明确标记时优先保留；只有裸 ephemeral 标记时可以继承更具体的顶层配置。
2. 补齐默认断点；顶层已经落到消息时，不再重复添加滚动消息断点。
3. 总预算为四个；非 null 的 `thread` 预留一个。只删除标记，不删除内容。仅统计真实的顶层、tools、system、消息内容位置，不把工具 schema 或 tool_result 数据中的同名字段算入。
4. 普通主请求为没有显式 TTL 的标记补一小时；子代理默认五分钟，只有调用方真实缓存标记请求一小时才保留该策略；探测/辅助请求另行处理。
5. 按 tools → system → messages → 尚存顶层遍历。出现五分钟/默认标记后，将后续一小时标记降为默认 TTL，不为迁就后面的标记而延长前面的显式五分钟缓存。

`prompt_cache_options.mode: "explicit"` 关闭自动布点与 TTL 改写，但不禁用缓存，也不免除断点预算。该内部选项发送前删除。显式模式的非法 TTL 或顺序组合本地报错，不静默改写。如果 Messages 探测/辅助请求禁止所需的 extended-cache beta，其显式一小时缓存也会在本地拒绝。

内部 `count_tokens` 改写分支不执行完整 Messages cloak、顶层物化或一小时升级。本次没有新增公开的 `/v1/messages/count_tokens` 路由；该分支以适配器单元测试覆盖。既有标记只参与统一预算，以及非 explicit 模式的 TTL 顺序规范化。已确认原生 Messages 也保留缓存布点的控制权，并维持现有限额/规范化行为及 explicit 例外。

## 最终校验和签名

非原生请求在 egress 准备结束时执行只读校验：

- 生成身份 Header 与 billing 的 CLI 版本一致；token 计数不要求 billing。
- 真实缓存位置的对象/type/TTL 合法性。
- 断点预算与混合 TTL 顺序。
- 一小时 Messages 缓存标记与 extended-cache beta 配套。

错误只指出字段，不包含请求文本或凭据。Dispatcher 在上游推理前返回本地 400，不因该错误换账号重试。校验不修改请求体；现有传输 hook 随后对最终序列化字节签名，并在响应中恢复工具别名。

Nyro 当前通道改写位于通用 payload/Header 准备**之后**，不是 CLIProxyAPI 独立的最终用户 payload 规则屏障。以后如增加更晚的覆盖，必须把只读校验移到或重做于新的最终边界；不要在显式用户覆盖之后偷偷再次规范化。

## 验证

```bash
cargo test -p nyro-core --lib provider::anthropic::claude_code
cargo test -p nyro-core --test claude_code_oauth
cargo check -p nyro-core
```

覆盖版本一致性、beta 样本、原生版本保留、顶层缓存、工具结果续轮、流式、explicit、探测/子代理、thread 预算、token 计数及本地拒绝不重放。Fixture 和静态 profile 均不代表完整复现官方 CLI 的所有行为。
