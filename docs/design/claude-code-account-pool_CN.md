# Claude Code OAuth 账号池

[English](<claude-code-account-pool.md>)

## 范围与兼容性

一个供应商仍是一个路由目标，其内部 Claude OAuth 账号独立管理、独立调度。本功能仅对 Anthropic 身份、`channel = "claude-code"` 且 `auth_mode = "oauth"` 的供应商开放。OAuth 驱动键为 **`claude-code`**，公开账号池类型为 **`claude_code`**，两种拼写不可混用。

普通 Anthropic API Key、其他 OAuth 渠道和创建供应商流程保持不变。未配置账号池的 Claude 单账号仍可正常使用和重新授权，无需开启账号池。保存调度设置后才能添加更多账号。UI 不需要新增 HTTP 或桌面 IPC 命令，也不需要新增数据库表。

## 管理界面

供应商卡片显示后端返回的账号数量、可用状态和全池用量刷新按钮。卡片与管理面板汇总框均列出已启用且未断开账号的识别套餐分布，各账号行也标注识别到的套餐。未知或未识别套餐明确标注按 Pro 估算，不冒充已验证的 Pro 订阅。**管理账号**与供应商编辑面板支持：

- 通过浏览器 OAuth 和授权码交换添加账号。
- 定向重新授权；打开浏览器前固定稳定账号 ID 与状态版本。
- 修改安全别名、整数优先级（越小越优先），启用/停用或移除账号。
- 刷新单账号或全池用量。
- 轮询 / 优先用满 / 最近周重置优先策略、会话粘性与 TTL、最大账号尝试数。
- 逐账号显示上游额度窗口和保留日志用量统计。

移除账号只清除凭据，在内部保留断开记录与版本围栏，防止旧 OAuth 回调恢复授权。已移除账号不再列于管理面板（也不再显示其逐账号统计行），保留的历史请求日志仍在。如果旧缓存返回断开行，UI 会禁止该行后续变更。报错和版本冲突后重新读取缓存，后续操作使用最新状态版本。所有账号行共用一次统计查询，仅展开管理面板时挂载，收起的卡片不读取账号统计。

Claude 设置始终提交 `reserve_policy = "disabled"`，不提供付费余额开关。上游 `extra_usage` 信息不足以证明付费容量可用，不能据此启用自动付费兜底。此设置**不是账单硬限额**：如需禁止额外支出，还必须关闭上游额外用量。

## 用量契约与展示

共享账号公开 DTO 新增以下可选字段：

```ts
claude_usage?: {
  plan?: string | null; // pro、max 5x、max 20x 或其他识别到的套餐
  windows: Array<{
    name: string;
    used_percent: number;
    resets_at: string | null;
  }>;
} | null;
```

套餐在刷新用量和账号授权时逐账号从上游 OAuth profile 识别。旧快照没有套餐字段仍可读取。缺失或未识别的套餐（包括无法识别档位的 Max、Team、Enterprise）在两个汇总窗口均默认按 Pro 权重 1 计算。这是显式的估算兜底，不表示已验证为 Pro，不修改识别到的套餐。不要求额外用量字段。原有账号授权状态、调度状态、观察时间和 stale 标记继续以后端为准。

规范窗口名称包括 `five_hour`、`weekly_limit` 和 `feature:<模型或功能>:weekly_limit`，例如 Sonnet、Opus、OAuth apps 或特定模型的周限制。后端规范化实际上游窗口，UI 逐项展示全部返回窗口，包括模型窗口。缺失窗口不会凭空补齐；缺失用量表示**未知**，不表示零。缺失或无效的重置时间显示未知；旧快照明确标记。时间到达重置点不会在本地恢复额度。

卡片与管理面板展示由 core 计算的**账号池加权估算**，通过 `quota.claude_windows` 返回 `{ name, quota: PoolQuotaWindow }` 数组，`quota.estimated` 为 true。两个窗口分别使用以下 Pro 等价容量权重：

| 窗口 | Pro | Max 5× | Max 20× |
|---|---:|---:|---:|
| `five_hour`（5 小时） | 1 | 5 | 20 |
| `weekly_limit`（每周） | 1 | 5 | 10 |

已用单位为 `sum(权重 × 已用百分比 / 100)`，全池已用百分比为 `100 × 已用单位 / sum(权重)`。周权重属于应用估算，不声称为官方承诺额度；这里使用 Claude 专属权重，不套用 Devin Max 倍率或 Kiro Credit。模型专属窗口仍逐账号展示，不合并。

汇总纳入已启用且未断开的账号，包括额度耗尽账号；不代表当前可调度容量。未知套餐在每个窗口按一个 Pro 等价容量单位计入；缺失、无效、过期用量使汇总百分比显示未知，不把该账号当成零用量，UI 同时展示有效用量覆盖率与已知容量。两个窗口独立判断完整性。各账号保留自己的重置时间；最近重置及涉及账号数不表示全池同时重置，不据此伪造共同窗口。两个汇总窗口在所有屏幕宽度均上下排列，进度条的匀速预估标记按各账号独立窗口已过时间及对应 Claude 套餐容量加权，并标注预估百分比；用量、成员、容量或重置时间覆盖不完整时隐藏标记。账号池及单账号匀速标记均仅表示视觉估算。加权汇总不改变调度决策。

保留日志统计复用原有账号 usage-stats 端点：全部保留记录和 **本周期**，后者从每个账号的 `weekly_limit.resets_at` 向前推 7 天，统计至重置时刻之前，且不超过当前时间。后端返回毫秒时间戳 `period_start`/`period_end`；重置时间缺失、无效或已过期时 `period` 为 null，不回退自然周，也不伪造零用量。界面以本地时间展示周期起止，并在周期未知或到期时提示刷新用量。Token 与等价 API 价格来自日志，不是上游额度或永久账单；不同币种分开展示，清理日志会减少累计值，失败/重试请求和缺失参考价格沿用原统计语义。

### 匿名用量转发与 DSH

Provider 用量新增 `pool_quota`，作为账号池配额的匿名、仅供展示副本，结构为
`daily`、`weekly`、`claude_windows`、`estimated`。多账号的 `tiers` 仍为空。
`/v1/nyro/usage` 对普通 Key 与特权 Key 均保留此摘要，不受 `balances`/`spends`
金额权限影响；`account_pool` 中的身份、配置及嵌套上游报告仍不转发。两端采用独立
配额 DTO；下游类型反序列化剔除嵌套未知字段，仅接受规范 Claude 池窗口名并校验
重置元数据。有效覆盖率、空百分比、旧数据与错峰重置语义保持不变；该字段不作为
调度观察输入。

DSH 优先显示 `pool_quota`，直连旧版 Admin 响应时兼容 `account_pool.quota`。
不读取转发账号明细，也不从最近一次账号重置推导共同窗口或匀速标记。旧转发报告
缺少配额时明确提示缺少摘要，不再误报金额未开放。摘要生成需要升级上游 Nyro；
部署与 DSH 插件构建、重载是独立步骤。

## API 与会话生命周期

WebUI 复用 [backend.ts](<../../webui/src/lib/backend.ts>) 中已有的命令映射：

| 操作 | HTTP 接口 |
|---|---|
| 读取账号池 | `GET /api/v1/providers/:id/accounts` |
| 配置调度 | `PUT /api/v1/providers/:id/account-pool` |
| 初始化账号授权 | `POST /api/v1/providers/:id/accounts/auth-sessions` |
| 绑定就绪账号会话 | `POST /api/v1/providers/:id/accounts/bind` |
| 修改账号 | `PATCH /api/v1/providers/:id/accounts/:account_id` |
| 移除账号 | `DELETE /api/v1/providers/:id/accounts/:account_id` |
| 刷新账号用量 | `POST /api/v1/providers/:id/accounts/:account_id/usage/refresh` |
| 刷新全池用量 | `POST /api/v1/providers/:id/account-pool/usage/refresh` |
| 保留日志统计 | `GET /api/v1/providers/:id/accounts/usage-stats` |

初始化账号会话时固定添加意图（`account_id` / `expected_version` 均为 null），或定向重新授权意图（两者均有值）。返回 Claude 浏览器授权地址以及 `completion_mode = "manual_exchange"`。用户打开页面登录目标账号，粘贴授权码（包括 `code#state`）或完整回调地址；UI 调用 `complete_oauth_session`，然后通过 `bind_provider_account` 消费就绪会话。**绑定请求只发送会话 ID 和可选别名，不重新选择账号 ID。** 目标与 CAS 校验由后端负责。

未配置的旧单账号重新授权使用 `init_oauth_session`，vendor 为 `claude-code`，保留供应商代理设置，完成后调用 `bind_provider_oauth`。UI 在授权前和绑定前均检查供应商仍未配置账号池、账号目标无歧义；后端也必须原子执行此约束。

共享 [OAuth 会话 hook](<../../webui/src/lib/use-oauth-session.ts>) 负责取消、过期、会话代次所有权、请求中止及过期回调拒绝。授权码在提交/取消时清空，不持久化到账号 DTO 或别名。后端拒绝重复 token；已知 subject 时，定向重新授权为不同身份会被拒绝，而非静默替换身份。

## 后端调度与恢复边界

与现有其他账号池一致，账号选择存在歧义时不支持未指定目标账号的模型生成探测，会明确返回 `AMBIGUOUS_ACCOUNT`，而不是静默选取账号。非生成式连接检测与模型发现仍可使用。

UI 不根据进度条推导可用性或重试。后端检查每个账号的 5 小时、全局周窗口及请求模型适用的窗口。Opus 专属窗口耗尽不能阻塞 Sonnet。

Claude 专属可选策略 `weekly_reset_first`（最近周重置优先）优先使用距离下次**全局周额度重置**（`weekly_limit.resets_at`）最近的账号，不按用量数据刷新时间或最近一次重置时间排序。现有可用性等级、手动优先级（越小越优先）和有效会话粘性仍优先；同等级、同优先级内按未来重置时间升序，缺失、无效或已过期的时间排后，相同时间再按创建时间与账号 ID 稳定排序。粘性账号仍位于首位，其余回退候选保留重置时间顺序。重置时间已过不会在没有上游观测时解除额度耗尽。默认仍为 `round_robin`；Devin/Kiro 账号池不能保存此策略。

Token 在请求前进入五分钟到期余量时刷新，采用逐账号版本/CAS 隔离；后台刷新同样隔离。只有结构化上游拒绝允许安全切换账号：`authentication_error`（401）、`rate_limit_error`（429）、`permission_error`（403）、`overloaded_error`（529 或 503）。裸 HTTP 状态、网络超时、已接受的流不得触发重放。Credits-required 可以尝试其他账号，但不能改写账号健康或额度。用量查询 401 需要重新授权；用量查询 403 保守地只标记观察数据为 stale。

## 实现与验证

- [Claude 账号池组件](<../../webui/src/components/claude-account-pool.tsx>)：卡片、管理、实际窗口展示、保留日志统计。
- [Claude 登录组件](<../../webui/src/components/claude-account-login.tsx>)与[会话辅助函数](<../../webui/src/lib/claude-account-login.ts>)：浏览器/授权码交换与不可改目标的会话意图。
- [共享调度表单](<../../webui/src/components/devin-pool-settings.tsx>)：Claude 的付费兜底限制，同时保留 Devin/Kiro 原行为。
- [UI 测试](<../../webui/src/components/claude-account-pool.test.tsx>)与[契约测试](<../../webui/src/lib/claude-account-login.test.ts>)：身份门控、窗口渲染、分窗口加权汇总、未知/旧数据、双语标签、保留统计、单账号兼容、目标/CAS 参数及取消防护。

使用项目已安装的 esbuild 编译相关 Node 测试。Vite 图标 glob 仅在测试产物中替换为空映射，不改变应用行为。同时回归 Devin/Kiro 账号池、HTTP 映射、provider-usage 与 usage-pace 测试。生产类型检查/构建不得覆盖受版本控制的生成文件：

```sh
# 在 webui/ 内运行；esbuild 路径对应当前已安装的锁文件版本
node_modules/.pnpm/esbuild@0.27.3/node_modules/esbuild/bin/esbuild \
  src/lib/claude-account-login.test.ts src/components/claude-account-pool.test.tsx \
  --bundle --platform=node --format=esm --packages=external --alias:@=./src \
  --jsx=automatic --loader:.png=dataurl \
  --banner:js='import.meta.glob = () => ({});' \
  --outdir=node_modules/.tmp/claude-pool-tests --out-extension:.js=.mjs
for suite in node_modules/.tmp/claude-pool-tests/lib/*.test.mjs \
  node_modules/.tmp/claude-pool-tests/components/*.test.mjs; do
  node "$suite" || exit $?
done
npm run build -- --outDir node_modules/.tmp/claude-pool-build
```

自动化检查不能代替真实账号验收。浏览器 OAuth、多个真实订阅计划、桌面 IPC、部署环境的流式/重试行为仍需专项验收；这些 UI 测试不声称完成真实账号授权。
