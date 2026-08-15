# Nyro Server

Nyro Server 是 Nyro AI Gateway 的独立服务端二进制，提供完整的 Admin API、WebUI 和数据持久化。

## 快速开始

```bash
nyro-server
```

默认使用 SQLite，数据目录 `~/.nyro`，代理端口 `19530`，管理端口 `19531`。WebUI 已内嵌在二进制中，无需额外部署，启动后访问 `http://localhost:19531` 即可。

### 从源码构建并手动启动

```bash
# 1. 进入项目目录
cd <nyro 仓库路径>

# 2. 构建(debug 或 release 二选一)
cargo build -p nyro-server               # debug,产物在 target/debug/nyro-server
# cargo build -p nyro-server --release   # release,产物在 target/release/nyro-server

# 3. 手动启动(默认 mode=all:代理 19530 + 管理 19531)
./target/debug/nyro-server \
  --data-dir ~/.nyro \
  --admin-token <你的管理token> \
  --log-level info
```

**验证启动：**

```bash
curl http://127.0.0.1:19530/health          # 代理面健康检查
curl http://127.0.0.1:19531/healthz         # 管理面健康检查
```

启动后打开 `http://127.0.0.1:19531`(WebUI)创建 provider 与模型即可开始使用；API 密钥只填写在 WebUI/管理 API 中，不会写入代码或版本历史。

> 如需无数据库的纯 YAML 配置模式，参见 [Standalone 模式](../standalone/README.md)。

---

## 命令行参数

### Server

| 参数 | 环境变量 | 默认值 | 说明 |
|------|----------|--------|------|
| `--proxy-host` | `NYRO_PROXY_HOST` | `127.0.0.1` | 代理监听地址 |
| `--proxy-port` | `NYRO_PROXY_PORT` | `19530` | 代理监听端口 |
| `--proxy-auth-key` | `NYRO_PROXY_AUTH_KEY` | 无 | 代理数据面强制 `Authorization: Bearer <key>`（健康检查端点保持开放）。代理监听非回环地址时建议必须设置 |
| `--admin-host` | `NYRO_ADMIN_HOST` | `127.0.0.1` | Admin API 监听地址 |
| `--admin-port` | `NYRO_ADMIN_PORT` | `19531` | Admin API 监听端口 |
| `--admin-token` | `NYRO_ADMIN_TOKEN` | 无 | Admin API Bearer Token 鉴权 |
| `--lan` | `NYRO_LAN` | `false` | 局域网模式：代理与 Admin 监听均绑定 `0.0.0.0`。要求同时设置 `--proxy-auth-key` 与 `--admin-token`，否则拒绝启动 |
| `--log-level` | `NYRO_LOG_LEVEL` | `info` | 日志级别：`error` / `warn` / `info` / `debug` / `trace` |

### Storage

| 参数 | 环境变量 | 默认值 | 说明 |
|------|----------|--------|------|
| `--data-dir` | `NYRO_DATA_DIR` | `~/.nyro` | 数据存储目录（SQLite 数据库存放位置） |
| `--storage-backend` | `NYRO_STORAGE_BACKEND` | `sqlite` | 存储后端：`sqlite` / `postgres` / `mysql` |
| `--migrate-on-start` | `NYRO_MIGRATE_ON_START` | `true` | 启动时对所有后端自动运行 schema 迁移；设为 `false` 可跳过 DDL，搭配 `--migrate-only` 独立执行迁移（适合 K8S 分权部署）|
| `--migrate-only` | — | `false` | 执行数据库迁移后退出，不启动服务（适合 K8S Job / initContainer）|
| `--postgres-dsn` | `NYRO_POSTGRES_DSN` | 无 | PostgreSQL 连接字符串（`--storage-backend=postgres` 时必填） |
| `--postgres-max-connections` | — | `10` | PostgreSQL 连接池最大连接数 |
| `--postgres-min-connections` | — | `1` | PostgreSQL 连接池最小连接数 |
| `--postgres-idle-timeout` | — | 无 | PostgreSQL 空闲连接超时（秒） |
| `--mysql-dsn` | `NYRO_MYSQL_DSN` | 无 | MySQL 连接字符串（`--storage-backend=mysql` 时必填） |
| `--mysql-max-connections` | — | `10` | MySQL 连接池最大连接数 |
| `--mysql-min-connections` | — | `1` | MySQL 连接池最小连接数 |
| `--mysql-idle-timeout` | — | 无 | MySQL 空闲连接超时（秒） |

### Advanced (CORS)

| 参数 | 说明 |
|------|------|
| `--admin-cors-origin` | Admin API 允许的 CORS 源（可重复，`*` 表示任意） |
| `--proxy-cors-origin` | 代理 API 允许的 CORS 源（可重复，`*` 表示任意） |

---

## 存储后端

### SQLite（默认）

无需额外配置，数据库文件存储在 `--data-dir` 下的 `gateway.db`：

```bash
nyro-server --data-dir ~/.nyro
```

### PostgreSQL

```bash
nyro-server \
  --storage-backend postgres \
  --postgres-dsn "postgres://user:pass@localhost:5432/nyro"
```

或通过环境变量：

```bash
export NYRO_STORAGE_BACKEND=postgres
export NYRO_POSTGRES_DSN="postgres://user:pass@localhost:5432/nyro"
nyro-server
```

### MySQL

```bash
nyro-server \
  --storage-backend mysql \
  --mysql-dsn "mysql://user:pass@localhost:3306/nyro"
```

或通过环境变量：

```bash
export NYRO_STORAGE_BACKEND=mysql
export NYRO_MYSQL_DSN="mysql://user:pass@localhost:3306/nyro"
nyro-server
```

---

## 局域网访问

默认所有监听都绑定回环地址，仅本机可访问。要让局域网内其他设备使用 Nyro，有两种方式：

### 方式一：`--lan` 一键开启（推荐）

```bash
nyro-server --lan \
  --proxy-auth-key "your-proxy-key" \
  --admin-token "your-secret-token"
```

`--lan` 会把代理（`:19530`）与 Admin/WebUI（`:19531`）监听都绑定到 `0.0.0.0`，并要求：

- `--proxy-auth-key`：代理数据面强制 Bearer 鉴权，局域网设备调用模型时必须携带 `Authorization: Bearer your-proxy-key`；
- `--admin-token`：Admin API 强制 Bearer 鉴权，WebUI 登录需要该令牌。

两者缺一即拒绝启动。客户端使用示例：

```bash
# 局域网内其他设备（假设主机 IP 为 192.168.1.10）
curl http://192.168.1.10:19530/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer your-proxy-key" \
  -d '{"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}]}'
```

### 方式二：手动指定监听地址

```bash
nyro-server \
  --proxy-host 0.0.0.0 \
  --admin-host 0.0.0.0 \
  --proxy-auth-key "your-proxy-key" \
  --admin-token "your-secret-token"
```

当 `--admin-host` 不是回环地址（`127.0.0.1` / `localhost` / `::1`）时，**必须**设置 `--admin-token`。代理监听非回环地址时强烈建议设置 `--proxy-auth-key`，否则任何能访问该端口的设备都可以无鉴权调用你的模型。

### 浏览器访问 WebUI

`--lan` 或非回环 Admin 地址下，局域网浏览器访问 `http://<主机IP>:19531` 时需要放行跨域来源（默认仅允许本机与 Tauri 来源）：

```bash
nyro-server --lan \
  --proxy-auth-key "your-proxy-key" \
  --admin-token "your-secret-token" \
  --admin-cors-origin "http://192.168.1.20:19531"   # 可重复；* 表示任意来源
```

命令行客户端（curl / Claude Code / SDK）不受 CORS 限制，无需配置此项。

---

## WebUI

WebUI 已内嵌在 `nyro-server` 二进制中，Admin 端口自动提供服务，无需额外部署。启动后访问：

```
http://localhost:19531
```

---

## 客户端调用

启动后，所有协议客户端均可通过代理端口访问已配置的路由：

```bash
# OpenAI 协议
curl http://localhost:19530/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4o", "messages": [{"role": "user", "content": "hello"}]}'

# Anthropic 协议
curl http://localhost:19530/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: any" \
  -H "anthropic-version: 2023-06-01" \
  -d '{"model": "gpt-4o", "max_tokens": 1024, "messages": [{"role": "user", "content": "hello"}]}'
```
