# 失败可观测性与尝试结果

[English](failure-observability.md) · [数据库结构](../database/schema.md)

本文描述请求日志的诊断契约，与 [observability.md](observability.md) 中尚处于提案阶段的 OpenTelemetry 框架分开。实现位于 `nyro-core` 的 `logging/diagnostics.rs`、`logging/payload.rs`、代理/响应生命周期观察器、存储层与管理服务；HTTP 管理接口与桌面管理接口共用核心语义。

## 范围与不变量

- 已确认失败、超时、取消、输出上限，即使普通载荷记录关闭，也保留有界诊断证据。
- 区分单次上游尝试失败与可能经过重试的客户端请求最终结果；最终结果行不能计入尝试统计。
- 缺失或不受支持的证据视为 **unknown（未知）**，而非成功或失败。
- 不改变既有 TPS 公式、token/配额计数、路由、重试策略或线上传输字节。诊断副本的大小限制不是请求/响应传输限制。
- 日志仍是有界、异步、尽力而为的机制；本功能不承诺无损持久化，也不承诺恢复历史正文或结果。

## 权威结果

只有 `outcome_version = 1` 是当前认可的 `attempt_outcome` 权威版本。它独立于 `performance_metadata_version`、`request_completion`、token 用量及其他旧性能证据。仅有版本号不代表成功，还必须有可识别的结果。

显示、错误计数以及 **清空错误日志** 共用以下有效错误判定：

```text
is_error = (400 <= client_status_code <= 599)
        OR (400 <= upstream_status_code <= 599)
        OR (outcome_version == 1 AND attempt_outcome IN {failed, timed_out})
```

NULL、600、未收到 HTTP 响应不满足 HTTP 错误条件。即使客户端为 HTTP 200，上游 429/500 仍算错误。真实 HTTP 错误对历史行、未知版本仍然有效。单独的旧 `request_completion = failed` 不足以确认错误。

分类互斥，按以下优先级计算：

| 有效结果 | 条件 |
|---|---|
| `error` | 满足上述错误判定，优先于其他所有结果 |
| `completed` | 非错误，版本 1，且 `attempt_outcome = completed` |
| `cancelled` | 非错误，版本 1，且 `attempt_outcome = cancelled` |
| `output_limited` | 非错误，版本 1，且 `attempt_outcome = output_limited` |
| `unknown` | 其他全部情况，包括没有权威证据、只有旧标记、不支持的版本/值 |

确认完成需要上游协议的有效终止证据，以及网关侧确认的完整投递：要么观察到响应 Body 正常结束（EOS），要么 HTTP 层已消费转发层产出的**全部客户端帧**（帧计数对账）。后者是必要的补充：SSE 客户端（如 codex CLI）读到协议终止事件后可以立即关闭连接，此时响应 Body 的终止 EOS 轮询与上游流的 EOF 轮询都会在竞态中落败——上游终止事件已解析、且客户端每一帧都已送达时，这些 drop 痕迹应被对账为确认完成，而非取消。HTTP 200、非零 token、转换层合成的完成标记都不够；Body EOS 也不是客户端应用的 ACK。已确认错误/超时不能被投递对账降级，也不会被后来的取消、输出上限或未知观察覆盖。纯取消、纯输出上限既不是确认成功，也不是错误。

当生命周期观察器确实获得证据时，受支持协议的显式错误、连接/读取/解压失败、转换失败、超时、缺少必需终止事件等可以确认失败。诊断解析窗口溢出、遇到不支持的协议方言，不能据此认定协议错误，应保持未知。载荷副本截断本身也不是传输或协议失败。

### 计数与比率

Provider、API Key、模型用量详情返回 `outcome_stats_version = 1` 和五个互斥计数：

```text
request_count = success_count + error_count + cancelled_count
              + output_limited_count + unknown_count

确认成功率 = success_count / request_count
未知率     = unknown_count / request_count
```

分母是选定时间窗口中 **全部保留的尝试**，不是仅已知结果，也不是去重后的客户端请求。零尝试时显示无数据，不能除零。`success_count` 只表示确认的 `completed`，不是 HTTP 2xx，也不是 `request_count - error_count`。界面同时显示未知计数/比率，避免把较低的确认成功率误解为较高的失败率。既有分组/时序的 `error_count` 共用同一判定；延迟、TPS、token 公式保持不变。

历史测试数据和迁移行保留缺少权威证据的事实。例如版本 0 的 HTTP 200、302、404、500 得到总数 4、成功 0、错误 2、未知 2。不能为了保住旧成功计数而补写版本 1。

## 标识、关联与请求最终结果

每条日志在持久化前分配稳定 `id`，builder/fallback 副本保留该标识。`client_request_id` 关联同一个客户端请求，`attempt_index` 区分其上游尝试。历史行可以缺少这两个字段。后续尝试成功不能抹去先前尝试的失败。

独立 `request_results` 表按 `client_request_id` 保存一份最终结果：`final_outcome`、`final_attempt_id`、`attempt_count`、`finished_at`。管理日志详情关联为可选的 `request_result`；它不是另一条请求日志，**不增加** 请求、成功、错误、TPS、token 或配额计数。最终记录未持久化时关联可能缺失，缺失不代表成功。`final_attempt_id` 故意不设置到日志的外键：删除最后一次尝试的日志时，只要其他关联尝试仍存在，最终结果不必随之丢失。

公共 `RequestLog` 包含派生的 `is_error`、`effective_outcome`，以及 `failure_kind`、`failure_stage`、有界且脱敏的 `error_message`、`error_causes`。管理接口中的 `error_causes` 和 `payload_metadata` 是可空的 **JSON 编码字符串**，不是内嵌数组/对象，客户端只解析一次；物理列名分别为 `error_causes_json`、`payload_metadata_json`。摘要/列表查询不返回原始载荷，经过授权的管理详情可查看保留的内容与关联信息。

## 有界载荷证据

四个正文方向独立捕获：客户端请求、上游请求、上游响应、客户端响应。每个方向：

- 最多保留 **1 MiB 原始字节**，由开头 **512 KiB** 与末尾 **512 KiB** 组成；较小正文保留全部已观察字节。
- 存储内容不插入伪造分隔符。被截断的正文是头尾片段，不是可以原样重放的完整正文；元数据说明分界。
- 保留字节不做有损修改。有效 UTF-8 保存为文本，否则使用 Base64；头尾截断切开 UTF-8 字符时也使用 Base64。编码字符串最大可为 1,398,104 字节，原始字节上限仍是 1 MiB。
- 断连/读取失败可能只留下部分数据。未观察到的方向标记缺失，不伪造空正文；明确观察到的空正文与缺失可区分。

四组请求/响应头分别以 **64 KiB 序列化 JSON** 为上限；共用实现实际最多保留 **65,535 字节**，兼容 MySQL `TEXT`，避免边界处仅该后端写入失败。先脱敏再保留；放不下的字段整项省略，保证 JSON 有效。协议、request ID、限流相关头优先；元数据说明省略/脱敏项，不能把不完整头部显示为完整。

最终保留开关为：

```text
retain = is_error
      OR (outcome_version == 1 AND attempt_outcome IN {cancelled, output_limited})
      OR (global_enable_payload AND model_enable_payload.unwrap_or(true))
```

已确认异常覆盖全局和模型的普通记录开关。普通 completed/unknown 仍受全局/模型开关控制，开启时使用 **相同上限**。普通流量的模型开关不能绕过关闭的全局开关。“强制保留”指保留实际观察到的有界证据，不保证四个完整正文，也不保证持久化成功。

### 元数据与界面解释

`payload_metadata` 以八个载荷字段名为键，例如 `client_request_body`、`upstream_response_headers`，每项说明：

- `capture_state`：`captured`、`empty`、`absent` 或 `not_retained`；
- `complete`：观察方向是否完整结束，与截断独立；
- `total_observed_bytes`、`retained_bytes`、`truncated`；
- `encoding`：按内容为 `utf8`、`base64` 或 `none`；
- 正文的 `head_bytes`/`tail_bytes`，或头部的 `total_headers`、`retained_headers`、`omitted_headers`、`redacted_headers`。

界面区分完整、部分、缺失、截断、Base64、管理员已清除，不能把每个非空字符串都当作完整正文。已观察字节数不推测从未接收到的字节。头部观察字节统计原始名称/值，保留字节统计脱敏后 JSON，二者不能直接按同一口径比较。元数据不存第二份原始正文。

## 隐私与清理操作

日志中的认证/cookie/自定义凭据头及携带凭据的 URL 要脱敏。错误链有界并经过清洗，不能成为绕过正文权限输出原始内容或密钥的另一条路径。**正文在上限内有意保留原文**，不递归清洗；提示词、响应、个人信息或正文中的凭据仍可能存在。仅授权管理接口允许查看；应妥善配置管理权限和保留期限。

**清空载荷** 清除所有存在载荷的日志中八个载荷列，**包括错误日志**。保留日志行、稳定 ID、分类、状态、关联、失败/捕获元数据、用量、耗时与路由元数据，并写入 `payload_cleared_at`（Unix 毫秒）。此后捕获元数据描述历史捕获，不代表当前仍可读取；界面应优先展示清除时间戳。已无载荷时重复清理不修改行。

**清空错误日志** 删除满足统一有效错误判定的行。不删除纯取消、输出上限或未知尝试。历史 HTTP 500 仍可删除，上游 HTTP 4xx 即使客户端 HTTP 200 也可删除。单条删除和清空全部日志仍是明确的独立操作。任何清理都不能重建已丢弃的证据。

## 持久化失败与限制

有界日志队列使用非阻塞入队。队列满、通道关闭时明确记录证据丢失事件，尽可能附带稳定日志/请求标识。数据库批量写失败也记录丢失事件，不隐式重试。进程内 `logging_status()` 提供 `queue_full_dropped`、`channel_closed_dropped`、`database_write_dropped`，并明确 `counts_reset_on_restart = true`。

本功能 **没有重试队列、持久化 spool 或历史正文/结果恢复**。数据库写失败计数按失败批次中的条目计算，不是持久审计账本，也不能证明驱动失败时具体哪些行已经提交。崩溃同样可能丢失在途证据，并重置计数；排查日志缺失时必须考虑这些限制。

## 验证与参考 SQL

既有 logging/admin/usage/time-series 测试验证保守历史计数、清空所有载荷、HTTP 错误边界；`log_outcomes_storage` 测试使用单独显式启用的空临时数据库验证版本化结果和存储一致性。只有最终迁移就绪后，才使用 `nyro-tools dump-schema` 生成 PostgreSQL/MySQL 参考 SQL，具体见 [安全生成手册](../database/schema.md#regenerating-reference-sql)。禁止手改生成 SQL，禁止把 schema/test 工具指向应用数据库。
