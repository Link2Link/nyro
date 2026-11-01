# 系统监控

[English](<system-monitor.md>)

侧栏新增的“系统监控”入口打开 `/system-monitor`，只读展示 **Nyro 运行环境**
的系统信息，而非浏览器所在设备。页面沿用控制台的圆角玻璃卡片，自动适配深浅主题，
支持英文和简体中文。

## 面板与操作

- CPU 使用率、逻辑/物理核心数量、型号、可获取的频率，以及可展开的逐核心占用。
- 内存已用/总量/可用量、Swap，以及 Nyro 进程内存、CPU、PID。
- 占用最高的磁盘与逐挂载点容量/可用空间进度条。挂载可能共享存储，因此不累加容量。
- 网络接收/发送吞吐趋势，以及逐接口累计流量。汇总排除回环接口
  （`lo`、`lo0`、`Loopback…`）；虚拟接口仍可能重复计入同一流量。
- 客户端至管理端的 HTTP 或桌面 IPC 往返耗时，含采集与序列化时间，
  **不是**上游模型延迟或 ICMP ping。
- 主机、操作系统、架构、内核及系统运行时间。

默认每 3 秒刷新，可选 5/10 秒；暂停仅停止自动轮询，仍可手动刷新。
后台标签页及离开页面后不持续轮询。刷新失败保留上次成功快照，但明确标记异常/过期，
不冒充实时数据。趋势在内存查询缓存中保留最近 60 次、最多 10 分钟的采样，
不写入数据库或持久化存储。长时间暂停或断线形成图表断点，不连续连线。

## 采集与接口

核心方法 `AdminService::get_system_status()` 位于
[system_status.rs](<../../crates/nyro-core/src/admin/system_status.rs>)，共用于：

- `GET /api/v1/system/status`：沿用管理端鉴权中间件和 `{ "data": … }` 响应封装。
- 桌面 IPC 命令 `get_system_status`。

进程共享采集器持有 `sysinfo` 的系统、磁盘、网络状态及 2 秒快照缓存。
刷新和锁操作均在 Tokio blocking pool 运行，不启动后台采集线程或定时任务。
CPU 与网络使用相邻采样差值，网络速率除以单调时钟实际间隔。
初次 CPU/网络速率、新接口速率及重置的计数器返回未知（`null`），不伪装为零；
真实空闲采样可以为零。首帧逐核心数据为空数组。进程 CPU 除以逻辑核心数，
按整机 0–100% 归一化。

响应包含 `sampled_at`（毫秒时间戳）、`sample_interval_seconds`、主机标识、
`cpu`、`memory`、`disks`、`networks` 及 `process`。容量以字节、速率以字节/秒、
频率以 MHz 表示；前端使用 IEC 容量单位。不可用或预热中的速率显示“—”。
采集不访问上游服务、不发起测速流量，也不修改数据库结构。

## 指标边界

容器中 CPU/内存可见范围取决于平台及运行时，不应假设等于 cgroup 配额。
磁盘和网络可见范围也取决于进程环境及权限。网络吞吐是实测使用量，
**不是**公网测速、购买带宽或物理链路上限。累计流量可能随接口或操作系统重启而清零。
监控不等同于健康认证，也不据此推断网关或供应商可用性。

## 验证

```bash
cargo test -p nyro-core system_status
cargo test -p nyro-server system_status
cargo check -p nyro-core -p nyro-server
(cd webui && npm run build)
node tests/webui/system-monitor-smoke.mjs
```

浏览器 smoke 通过 Chromium/CDP 由构建产物及确定性 fixtures 拦截响应所有页面、
资源和 API 请求，不启动 HTTP 服务，也不使用现有实例。
Chromium 前提条件见[浏览器测试说明](<../../tests/webui/README.md>)。
