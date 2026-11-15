# Google 订阅目录策略

本文仅覆盖 Google Antigravity 与 Gemini CLI 目录发现，不改变 Gemini codec、数据库 schema、开户策略或强制配额摘要端点。

## 端点与身份

- Antigravity 推理与 `fetchAvailableModels` 默认使用 `https://daily-cloudcode-pa.googleapis.com`。
- Gemini CLI 默认使用 `https://cloudcode-pa.googleapis.com`。
- provider base URL 不同于历史 prod 预设时，视为显式配置，由 Google auth 运行绑定同时用于推理与目录。已有 Antigravity prod 预设仍属于历史默认，不能判定用户有意选择 prod。
- Admin 发现、probe、用量查询遵循有效运行绑定的 base URL 与 User-Agent，不静默切换主机重试。OAuth refresh 通过 context 接收有效端点；初次 OAuth exchange 使用渠道默认端点。
- 内部 credential metadata 可保留显式 `subscription_endpoint`，这是集成细节，不是新增 UI 设置。

## 快照生命周期

仅刷新流程增加预算以保护已成功轮换的 token：token 准备阶段（包含可能的 Hub manifest 工作）限时 20 秒；之后可选的 project 恢复与目录发现共用 2 秒尽力预算。补充信息失败或超时仍保留成功的 token bundle；仍缺 project 时由请求阶段明确诊断。初次 OAuth exchange／开户行为不变。

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

## Probe 诊断

现有 error 字符串附带公共模型 ID、实际 wire model ID、端点来源；运行 metadata 原有 base URL 给出实际端点。Debug 日志记录模型映射和端点来源，不记录凭据指纹或 project ID。404 提示列举模型、端点／project、客户端版本、账户资格等可能原因，不断言某套餐不可用。配额读数同样不能单独证明 429 原因。

## 离线回归

工作区根目录串行执行。新增 HTTP fixture 只绑定 loopback，使用合成凭据，不访问真实 Google 端点／账户。

```sh
cargo test -p nyro-core google_catalog
cargo test -p nyro-core provider::google::antigravity::tests::catalog_
cargo test -p nyro-core google_subscription_
cargo test -p nyro-core subscription_model_catalog_keeps_wire_ids_and_channel_specific_fallbacks
cargo check -p nyro-core
```

覆盖 daily/prod 默认、历史 prod 兼容、显式 provider/runtime 绑定、请求路径/project/UA、无效与空响应区别、端点/project/UA/token 失效、auth/admin 来源一致性、失败保留、成功空目录 probe 行为、异步获取与重新绑定竞态。Storage CAS 测试另覆盖旧／refreshing generation 和 account state 保留。不证明线上模型可用性或套餐资格。
