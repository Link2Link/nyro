# OAuth 刷新安全

[English](<oauth-refresh-safety.md>)

## 范围

Memory、SQLite、PostgreSQL、MySQL 复用既有凭据行和版本 CAS，不新增数据库结构迁移或第二份令牌存储。刷新专用 HTTP 客户端禁止重定向和传输层自动重试，授权码交换不变。

令牌获取和结果发布在有时间上限的独立任务中运行。客户端取消只停止等待，不取消轮换或发布。发送前重新检查任务仍持有精确的 refreshing 版本。

## 错误分类

- **发送前失败**：本地构造或发送前连接建立失败，可以重试。
- **明确拒绝**：标准 OAuth 拒绝，凭据进入 error，需要重新授权。
- **刷新限流**：HTTP 429 将不含秘密的 RFC3339 deadline 存入 `nyro_refresh_retry_at`。供应商/账户刷新 CAS 在存储锁内检查，覆盖管理和后台入口。支持 Retry-After 秒数/HTTP 日期及 Retry-After-Ms，范围 1 秒至 1 小时，缺省 60 秒。
- **结果未知**：上游可能已轮换 grant，但网络、读响应、解析、超时或发布失败。Codex 隔离而不自动再次使用 grant。非 Codex 保留原临时错误策略，发布失败除外。

Codex/OpenAI 陈旧 refreshing 行在崩溃恢复时转为 error：超时不等于上游未收到 grant。其他驱动保持原有陈旧租约恢复策略。类型化刷新错误不保留原始令牌响应、秘密或请求 URL。

## 发布与限制

发布采用独立且有限的预算。重试只写同一 bundle 和版本 CAS，绝不再次发令牌请求。读取到携带相同令牌对、身份元数据及唯一 publication ID 的精确后继版本，可以确认写入成功但确认丢失。新授权会清除该 ID，即使令牌恰好相同也不能冒充旧发布。断开、重新绑定和新版本始终隔离旧任务。成功轮换/重新授权清除限流屏障；绑定失败补偿恢复原屏障。

**本实现不是全局 refresh journal。** 如果存储持续不可用或进程在发布前退出，没有独立持久化的 receipt 保存新 bundle；Codex 停止自动重试并要求重新授权。不同供应商或数据库导入相同 grant 不会全局去重，保护范围是既有账户行及其版本。

## 恢复

429 等待 deadline 即可，重复点击刷新不能绕过。Codex 结果未知时，重新登录并绑定新凭据，不要手动把状态改回 connected 或重用旧 refresh token。Claude 刷新限流不代表仍有效的 access token 被撤销，保留既有安全路由行为。

## 验证

新 fixture 仅使用合成凭据和 loopback HTTP/假驱动，不需要生产令牌。

```sh
cargo test --offline -p nyro-core --lib refresh_safety_tests
cargo test --offline -p nyro-core --lib auth::refresh
cargo test --offline -p nyro-core --lib refresh_http
cargo test --offline -p nyro-core --lib storage::oauth_refresh_tests
cargo check --offline -p nyro-core
```

覆盖响应丢失、损坏 JSON、明确拒绝、Retry-After 格式、取消、重新授权与发布竞争、Codex 陈旧租约、metadata-only CAS 和跨账户屏障隔离。
