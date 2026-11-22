# Google 订阅目录策略

本文说明 Google 两个订阅渠道的目录发现，以及配套测试覆盖的 Antigravity 登录、会话与限流行为。

## 端点与身份

- Antigravity 推理与 `fetchAvailableModels` 默认使用 `https://daily-cloudcode-pa.googleapis.com`。
- Gemini CLI 默认使用 `https://cloudcode-pa.googleapis.com`。
- provider base URL 不同于历史 prod 预设时，视为显式配置，由 Google auth 运行绑定同时用于推理与目录。已有 Antigravity prod 预设仍属于历史默认，不能判定用户有意选择 prod。
- Admin 发现、probe、用量查询遵循有效运行绑定的 base URL 与 User-Agent，不静默切换主机重试。OAuth refresh 通过 context 接收有效端点；初次 OAuth exchange 使用渠道默认端点。
- 内部 credential metadata 可保留显式 `subscription_endpoint`，这是集成细节，不是新增 UI 设置。

项目发现（`loadCodeAssist`）和首次开户（`onboardUser`）共用渠道的可信控制面：Antigravity 用 daily，Gemini CLI 用 prod。自定义推理或目录地址不会改变这两个请求的目的地。即使 HTTP 返回 200，只要已完成的开户操作带有错误，就报告开户失败，而不是缺 project。诊断提示 `VALIDATION_REQUIRED` 等已知原因，不回显上游消息或签名链接。

## 快照生命周期

仅刷新流程增加预算以保护已成功轮换的 token：token 准备阶段（包含可能的 Hub manifest 工作）限时 20 秒；之后可选的 project 恢复与目录发现共用 2 秒尽力预算。补充信息失败或超时仍保留成功的 token bundle；仍缺 project 时由请求阶段明确诊断。这两项预算不适用于首次登录。

credential metadata 同时保存 `subscription_models` 和 `subscription_models_source`。来源格式版本 1 将 ID 绑定到端点、project ID、完整 User-Agent（含版本）及 token 的 SHA-256 指纹。指纹不是 token，也不会出现在 probe 诊断中。缺来源或来源不匹配的旧快照视为未知；更换 project、端点、客户端身份或 token 后不复用其动态 ID。curated 别名与手动模型 ID 仍可使用。

Auth 与 admin 写入相同格式。Admin 在异步获取**之前**读取 connected row 和 generation，确认与 runtime 一致后，用 `update_meta_if_version` 更新 metadata。期间发生 refresh、退出并重新绑定或并发目录写入，会阻止旧结果写回。CAS 不替换 token、不重置 account state、不改变 refresh 时间戳；相同 metadata 不重复写入。

| 获取结果 | 动态快照 | 调用者语义 |
| --- | --- | --- |
| 有效非空 models map | 替换 ID 与来源 | 成功发现 |
| 有效空 models map | 用空数组与来源替换旧 ID | 成功空目录，不是故障 |
| HTTP 失败、JSON/map/entry 无效、超时 | 不改快照 | 尽力模式未知／回退；严格目录调用报错 |

新成功目录即使为空，也替代 probe 旧动态证据。失败可复用来源仍匹配的上次成功快照。空目录或模型未列出都不证明无权限；curated 发现条目保留，手动选择模型仍按请求探测。

## 线上观察：Gemini 4 Hydrogen / Argon（2026-11-15）

从美国 VPS 出口、Hub UA `antigravity/hub/2.32.1`，对两个 Google AI Pro 账号实测：

- 已开通账号、daily host：目录列出 `gemini-4-hydrogen-{low,medium,high,xhigh}` 与 `gemini-4-argon-{low,medium,high}`（另有 Ultra 专属占位 `gemini-4-carbon-disabled`）。各档位 `streamGenerateContent` 均返回 200；裸 ID `gemini-4-hydrogen` / `gemini-4-argon` 返回 404。因此裸名只作虚拟别名，按推理强度解析（`xhigh`/`max` 仅在 Hydrogen 上映射 `-xhigh`；无推理指令 → `-medium`）。
- 同一账号、prod host：目录不含 Hydrogen，Hydrogen 推理返回 429。这正是目录发现必须跟随 daily 推理 host 的原因。
- 配额：Hydrogen 调用消耗 `gemini-5h` / `gemini-weekly`（“Gemini Models”，与 3.8 Flash 共用）；Argon 调用只消耗 `gemini-argon-wtu-weekly`。
- 未开通账号：只有 `gemini-4-{argon,hydrogen,carbon}-disabled` 占位，档位 ID 返回 404。
- 系统提示词审查：`systemInstruction` 中出现第三方 Agent 身份（“You are an AI agent powered by DeepSeek Harness.”、Claude Code 身份）时，所有模型都返回不带细节的 429 `RESOURCE_EXHAUSTED`；同样文字放在首条 user 消息中可正常服务。因此 nyro 在该渠道始终改写 `systemInstruction`，且不带细节的 429 不再冷却模型。并非万无一失：Codex CLI 身份放在任何位置都会被拒。

## Neon 目录与用量展示

Antigravity 目录还报告了 `gemini-4-neon-{low,medium,high,xhigh}`。Nyro 折叠为 `gemini-4-neon`，按请求推理强度选择档位，无指令默认 `-medium`。三池用量视图将 Neon 与 Hydrogen 放在 **Gemini Models**，详见[配额桶说明](../design/quota-buckets_CN.md)。Neon 映射由离线测试覆盖，不属于上面的 11 月 15 日线上探测。

## Probe 诊断

现有 error 字符串附带公共模型 ID、实际 wire model ID、端点来源；运行 metadata 原有 base URL 给出实际端点。Debug 日志记录模型映射和端点来源，不记录凭据指纹或 project ID。404 提示列举模型、端点／project、客户端版本、账户资格等可能原因，不断言某套餐不可用。配额读数同样不能单独证明 429 原因。

## 离线回归

- Antigravity 保留原生 Gemini 显式传入的 `sessionId`。否则用客户端对话标识或第一段非空用户文本生成，按 provider、账号和 project 隔离。追加消息或续期 token 不会改变 ID；两个输入都没有时生成新 ID。
- HTTP 429 优先采用响应体的结构化等待时间，再读 `Retry-After`（整数秒或 HTTP 日期）。明确给出 0 秒不新增冷却；没有配额证据的裸 `RESOURCE_EXHAUSTED` 不冷却模型。明确给出的等待只作用于当前账号代际和实际出站模型，本地保护上限为七天。
- HTTP 200 的 Gemini 流中出现错误帧即为终态。尚未对客户端发送数据或正在聚合时返回错误响应；已开始发送时，输出客户端协议对应的错误事件，不追加成功标记，也不重放请求。

工作区根目录串行执行。新增 HTTP fixture 只绑定 loopback，使用合成凭据，不访问真实 Google 端点／账户。

```sh
cargo test -p nyro-core google_catalog
cargo test -p nyro-core provider::google::antigravity::tests::catalog_
cargo test -p nyro-core google_subscription_
cargo test -p nyro-core --lib auth::drivers::google::tests
cargo test -p nyro-core --lib antigravity_session_is_stable
cargo test -p nyro-core --test google_subscription_non_stream
cargo test -p nyro-core subscription_model_catalog_keeps_wire_ids_and_channel_specific_fallbacks
cargo check -p nyro-core
```

覆盖 daily/prod 默认、历史 prod 兼容、显式 provider/runtime 绑定、请求路径/project/UA、无效与空响应区别、端点/project/UA/token 失效、auth/admin 来源一致性、失败保留、成功空目录 probe 行为、异步获取与重新绑定竞态。Storage CAS 测试另覆盖旧／refreshing generation 和 account state 保留。不证明线上模型可用性或套餐资格。
