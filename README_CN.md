> 本仓库是 Nyro Rust 版本的独立维护分支，与原维护者不存在隶属或官方授权关系。
> 上游 Go 重写版本保留在 `upstream-go` 分支。

<p align="center">
  <img width="120" src="docs/images/NYRO-logo.png">
</p>

<h2 align="center">Nyro AI Gateway</h2>

<p align="center">
  让你的 AI 编码工具运行在任意模型、任意提供商之上。<br>
  一个网关，兼容所有协议，无需改代码。
</p>

<p align="center">
  <a href="https://github.com/Link2Link/nyro/releases/latest"><img src="https://img.shields.io/github/v/release/Link2Link/nyro" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache%202.0-blue.svg" alt="License"></a>
  <a href="README.md"><img src="https://img.shields.io/badge/Language-English-2d7ff9" alt="English"></a>
</p>

---

<p align="center">
  <img src="docs/images/NYRO-ui-home-cn.png" width="800">
</p>

---

## Nyro 是什么？

Nyro 是一个本地 AI 网关，位于你的 AI 工具与模型提供商之间。它可实时转换协议格式，因此 Claude Code、Codex CLI、Gemini CLI、OpenCode，以及任何使用 OpenAI / Anthropic / Gemini SDK 的客户端，都可以在不改一行代码的前提下，路由到你指定的任意后端模型。

将客户端指向 `http://localhost:19530`，其余交给 Nyro。

```
Claude Code · Codex CLI · Gemini CLI · OpenCode
     OpenAI SDK · Anthropic SDK · Gemini SDK
              任意 HTTP API 客户端
                      ↓
              Nyro AI Gateway
            (localhost:19530)
                      ↓
   OpenAI · Anthropic · Google · DeepSeek
    MiniMax · xAI · GLM · Xiaomi MiMo
       Kimi · Ollama · ...
```

Nyro 同时提供 **桌面应用**（macOS / Windows / Linux）和 **独立服务端二进制**，适用于无头部署与自托管场景。

---

## 为什么选择 Nyro？

**任意工具配任意模型。** Claude Code 使用 Anthropic 协议，Codex CLI 使用 OpenAI Responses API，Gemini CLI 使用 Gemini 协议。Nyro 可以在三者之间相互转换，让同一个模型同时服务所有工具。

**切换提供商不改工具配置。** 在 Nyro UI 中更换目标模型或提供商即可，工具侧配置保持不变。

**全程本地。** API Key 本地存储。请求仅在你的设备上流转，无云中转、无共享基础设施。

**一个 UI 管全部。** 在同一界面管理提供商、路由、API Key、日志与用量统计，可在桌面应用或浏览器访问。

---

## 界面截图

<table>
  <tr>
    <td align="center" width="50%"><img src="docs/images/NYRO-ui-providers-add-cn.png" height="260"><br><sub>Provider 管理</sub></td>
    <td align="center" width="50%"><img src="docs/images/NYRO-ui-routes-cn.png" height="260"><br><sub>路由配置</sub></td>
  </tr>
  <tr>
    <td align="center" width="50%"><img src="docs/images/NYRO-ui-apikeys-cn.png" height="260"><br><sub>API Key 管理</sub></td>
    <td align="center" width="50%"><img src="docs/images/NYRO-ui-connect-cn.png" height="260"><br><sub>Connect：代码与 CLI 集成</sub></td>
  </tr>
</table>

---

## 功能特性

### 协议转换

- **入口协议**：OpenAI（Chat Completions + Responses API）、Anthropic Messages、Gemini GenerateContent
- **出口协议**：可路由到任意 OpenAI 兼容、Anthropic 或 Gemini 上游
- **流式响应**：完整 SSE 透传与跨协议格式转换
- **推理内容**：支持 `<think>` 标签解析与跨协议转换
- **工具调用**：跨协议 tool call 与结果格式归一化
- **路由类型**：支持 chat 和 embedding 两种路由类型

### Devin CLI（实验性）

新增 Provider 时选择 **Devin → Devin CLI 订阅**，打开登录页面后将授权码粘贴回 Nyro；也可在同一个掩码输入框中导入已有的 `devin-session-token$…`。此接入使用 Devin CLI 的 PKCE 登录和 Codeium 原生 Connect/protobuf 聊天接口，**不是** Devin 公开的 sessions API，也不依赖 OpenAI 兼容中转；不会启动本地 Devin CLI 或执行本地工具。

- 固定上游为 `https://server.codeium.com`，协议为 `devin-chat/get-chat-message/v1`；不支持 API Key、自适应协议或自定义上游地址。
- 创建模型路由并指向此 Provider，例如 `swe-2`（默认映射到 `swe-2-high`）。客户端仍使用 Nyro 的 OpenAI Chat/Responses、Anthropic 或 Gemini 入口，支持流式和非流式响应。
- 内置模型列表仅是建议，**不代表实时账户权限列表**。可编辑 **Devin 模型 UID**，填写账户实际支持的精确模型名称。显式带思考等级的模型名优先于请求体设置，已知基础名称会映射到上游变体。
- 已知基础名称会按请求自身的思考强度映射到对应的上游变体，因此一条路由即可覆盖全部档位。涵盖 `claude-opus-5-5`、`claude-sonnet-5-5`、`claude-fable-5-1`、`gpt-6-astra`（`low`/`medium`/`high`/`xhigh`/`max`）、`gemini-4-argon`（`low`/`medium`/`high`），以及此前已支持的 `swe-2`、`gpt-5-2`、`gemini-3-8-flash`。对这五个新增系列，该系列未提供的强度会就近钳制（关闭思考和 `minimal` → `low`；`gemini-4-argon` 上的 `xhigh`/`max` → `high`），请求未携带任何思考指令时使用系列默认档（`medium`；`gemini-4-argon` 为 `high`）。显式变体（如 `claude-opus-5-5-high`）原样发送，不会被重新映射。
- 支持文本、函数工具调用/结果、思考内容/签名、内联 base64 图片和上游返回的 Token/缓存用量。远程图片 URL、embedding、托管工具、严格/强制工具约束和结构化输出会明确拒绝，不会静默丢弃。工具参数及思考后的文本可能缓冲到终止帧验证完成后返回。默认生成预算为 64,000 Token，显式预算原样传给上游。
- **用量查询**复用同一个鉴权后的 `GetUserStatus` 调用：返回套餐名、**每日**与**每周**额度窗口（上游给出剩余值，界面按已用显示）以及**超额余额**。超额余额只在窗口用完后才生效，因此它决定可用性：窗口用完时，**仅当超额余额为 $0** 才会暂停调度并等待重置；余额大于 0 时该 Provider 仍可调度，并显示“正在使用超额余额”提示，在 `usage` 均衡策略下排在所有仍有套餐内额度或用量未知的 Provider 之后（超额按量计费，仅作最后备选）。按积分计费的套餐不返回额度窗口。未返回周窗口时，`usage` 策略按每日窗口给它评分，与其他单窗口 Provider 一致。
- **连接测试**仅调用鉴权后的 `GetUserStatus`，不生成聊天；**模型测试**会发送短聊天，可能消耗额度，单次最多测试 16 个模型。Session Token 没有刷新授权，失效后请使用**重新授权**。

这是参考 CLIProxyAPI 已观察到的 Devin 协议实现的实验性接入；可用性及模型权限取决于 Devin 账户，上游协议变化可能需要更新适配器。鉴权和协议行为由本地模拟测试覆盖，订阅的端到端可用性仍需使用真实账户验证。

### 路由能力

- 基于 `virtual_model` 的精确匹配路由
- 通过虚拟模型名解耦客户端请求与真实后端模型
- 多目标路由，支持加权负载均衡（weighted）、优先级失败转移（priority）、延迟优先（latency）和上游套餐用量优先动态分流（usage）四种策略
- 可选的模型级降级兜底：标记一个 backend 行作为最后手段，仅在所有正常目标不可用时才调用
- 健康感知：连续 3 次失败标记目标不健康，30s 后自动恢复
- 按路由进行 API Key 访问控制

### 缓存能力

- **精确缓存**：完全相同的请求直接返回缓存结果
- **语义缓存**：基于 embedding 向量相似度匹配，相似请求复用缓存
- 支持按路由独立配置 TTL 和相似度阈值
- 缓存命中时支持流式回放

### 模型能力识别

- 自动检测模型能力（工具调用、推理、上下文窗口、输入输出模态、成本）
- 内置 `ai://models.dev` 数据源，支持离线能力查询
- 支持 HTTP 模型列表端点动态发现
- 路由配置界面展示能力标签辅助选择

### 安全能力

- 代理层与管理层 Bearer Token 独立控制
- 默认拒绝策略：API Key 必须显式绑定路由才可访问
- 单 Key 配额：RPM / RPD / TPM / TPD

### 管理能力

- Provider、路由、API Key 的完整 CRUD
- 请求日志记录 Provider、模型、Token、延迟等信息
- 按模型与提供商的用量统计图表；UTC 每日消耗日历、月度趋势与模型明细支持 **Token / 等值费用** 切换，并记住视图选择。等值费用按当前本地参考目录的每百万 Token 单价，分别计算普通输入、缓存读取与输出，缓存不重复计费。USD 与 CNY 按固定汇率 1 USD = 7.0 CNY 合并汇总，切换币种仅换算显示同一份合并总额。未知价格显示未计价而非免费；无精确价格的模型继承最长有效前缀匹配（如 `claude-haiku-5-5-medium` 继承 `claude-haiku-5-5`）。紧凑费用列表将 `claude-sonnet-5-5`、`claude-sonnet-5-5-max` 等带版本号的 Claude 推理档位变体及其跨提供商用量合为一个模型总费用，不改变逐项计价结果。默认展示费用最高的 5 个模型，可查看全部；展开行可看提供商 / 变体明细与原生单价。这是保留日志对应的 API 价值估算，不是实际账单或订阅节省额；未单独核算缓存写入溢价、长上下文阶梯、折扣及非 Token 收费。
- **手动设置 API 单价**：在「统计 → 等值费用」中展开模型费用行，再点击具体提供商 / 变体的「设置价格」。默认跟随参考目录，可按精确的提供商 / 上游模型组合设置美元或人民币的输入、缓存读取、输出单价（每百万 Token），也可确认后恢复默认。保存的规则同时作用于同提供商下以该模型为前缀的变体，除非变体单独设置了价格（如 `claude-haiku-5-5` 覆盖 `claude-haiku-5-5-medium`）；编辑弹窗会展示继承的单价及其来源模型。缓存单价留空表示跟随输入价，填写 0 表示免费。自定义价格保存到网关设置（数据库存储模式下持久化），优先于自动价格，历史及后续保留用量会重新估值，不修改日志或上游收费。未计价模型也可手动补价；配置导出 / 导入会将价格重新关联到新导入的提供商；因名称冲突而跳过的提供商保留现有价格。
- Provider 连通性测试与实时反馈
- 配置导入 / 导出

### Connect — 集成

**代码集成**：选择路由后一键复制可直接使用的示例：

| 协议 | 语言 |
|---------|---------|
| OpenAI | Python · TypeScript · cURL |
| Anthropic | Python · TypeScript · cURL |
| Gemini | Python · TypeScript · cURL |

**CLI 集成**：一键同步 AI 编码工具配置：

| 工具 | 协议 |
|------|---------|
| Claude Code | Anthropic |
| Codex CLI | OpenAI Responses API |
| Gemini CLI | Gemini |
| OpenCode | OpenAI |

Nyro 会自动检测已安装工具，为所选路由生成正确配置并一键写入，同时自动备份原始配置。

### 部署形态

**桌面应用**

| 平台 | 架构 |
|---|---|
| macOS | Apple Silicon (aarch64) · Intel (x64) |
| Windows | x64 · ARM64 |
| Linux | x86_64 · aarch64 |

**服务端二进制** — 支持三种运行模式，适配从单机到分布式的各类场景

| 模式 | 启动方式 | 用途 |
|---|---|---|
| `all`（默认）| `--mode all` | Proxy + Admin API + 内嵌 WebUI，单进程全功能 |
| `proxy` | `--mode proxy` | 仅代理调度，无 Admin、无 WebUI，适合纯流量转发节点 |
| `admin` | `--mode admin` | 仅 Admin API（+ 可选 WebUI），适合公司内部系统对接控制面 |
| Standalone | `--config config.yaml` | 纯 YAML 配置，无 DB、无 Admin，适合边缘/最小化部署 |

| 平台 | 架构 | 访问 |
|---|---|---|
| macOS | x86_64 · aarch64 | Proxy `:19530` · WebUI `http://localhost:19531` |
| Linux | x86_64 · aarch64 | Proxy `:19530` · WebUI `http://localhost:19531` |
| Windows | x64 · ARM64 | Proxy `:19530` · WebUI `http://localhost:19531` |

---

## 安装

### 桌面应用

**Shell 脚本**

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/Link2Link/nyro/master/scripts/install/install.sh | bash

# Windows (PowerShell)
irm https://raw.githubusercontent.com/Link2Link/nyro/master/scripts/install/install.ps1 | iex
```

**手动下载**

从 [GitHub Releases](https://github.com/Link2Link/nyro/releases/latest) 下载你平台对应的最新安装包。

> **macOS**：手动安装后请运行 `sudo xattr -rd com.apple.quarantine /Applications/Nyro.app`，或使用安装脚本自动处理。
>
> **Windows**：若 SmartScreen 提示 "Unknown publisher"，点击 **More info → Run anyway**。

### 服务端二进制

```bash
# 下载
curl -LO https://github.com/Link2Link/nyro/releases/latest/download/nyro-server-linux-x86_64
chmod +x nyro-server-linux-x86_64

# 启动（仅 localhost，无需鉴权）— 默认 all 模式
./nyro-server-linux-x86_64

# 启动（局域网访问，强制鉴权：代理数据面 Bearer + Admin 令牌）
./nyro-server-linux-x86_64 \
  --lan \
  --proxy-auth-key YOUR_PROXY_KEY \
  --admin-token YOUR_ADMIN_TOKEN

# 仅代理模式 — 只启动 :19530，无 Admin API、无 WebUI
./nyro-server-linux-x86_64 --mode proxy

# 仅 Admin 模式 — 只启动 :19531（供内部系统对接控制面）
./nyro-server-linux-x86_64 \
  --mode admin \
  --admin-host 0.0.0.0 \
  --admin-token YOUR_ADMIN_TOKEN

# Standalone 模式（YAML 配置，无 DB/Admin/WebUI；忽略 --mode）
./nyro-server-linux-x86_64 --config config.yaml
```

局域网访问时，其他设备将客户端指向 `http://<主机IP>:19530`，并携带 `Authorization: Bearer YOUR_PROXY_KEY`。详见 [Server 文档](docs/server/README.md) 的「局域网访问」章节。

可用服务端二进制：`linux-x86_64`、`linux-aarch64`、`macos-x86_64`、`macos-aarch64`、`windows-x86_64.exe`、`windows-arm64.exe`

打开 `http://localhost:19531` 进入管理界面。详细配置参见 [Server 文档](docs/server/README.md) 和 [Standalone 文档](docs/standalone/README.md)。

### 本地 Docker 镜像

在 Linux amd64 或 arm64 主机上，先构建服务端，再把编译结果打包成本地 Docker 镜像和归档文件：

```bash
make server
./make-docker.sh
docker compose up -d
```

`make-docker.sh` 会构建 `nyro:mine` 镜像、导出 `nyro-mine.tar`，并在 `.env` 不存在时创建该私有文件及随机 `NYRO_ADMIN_TOKEN` 与 `NYRO_PROXY_AUTH_KEY`（旧 `.env` 会自动补上缺失的 key）。Docker 端口默认发布到 `0.0.0.0`（局域网可访问），因此代理数据面强制 Bearer 鉴权：客户端需携带 `Authorization: Bearer <NYRO_PROXY_AUTH_KEY>`。打开 `http://localhost:19531`，使用 `.env` 中的令牌登录。

如需在另一台兼容的 Linux 主机上导入归档，请将 `compose.yaml` 放在归档旁边，创建包含强随机 `NYRO_ADMIN_TOKEN` 与 `NYRO_PROXY_AUTH_KEY` 的 `.env`（也可运行 `./make-docker.sh` 自动生成），先执行 `docker load -i nyro-mine.tar`，再执行 `docker compose up -d`。归档内是本机架构的原生二进制，因此目标主机必须使用相同的 CPU 架构。

### SQL 存储后端

默认使用 `--data-dir` 下的本地 SQLite。如需切换到 PostgreSQL 或 MySQL：

```bash
# PostgreSQL
./nyro-server-linux-x86_64 \
  --storage-backend postgres \
  --postgres-dsn "postgres://user:pass@host:5432/db"

# MySQL
./nyro-server-linux-x86_64 \
  --storage-backend mysql \
  --mysql-dsn "mysql://user:pass@host:3306/db"
```

或通过环境变量：

```bash
export NYRO_POSTGRES_DSN="postgres://user:pass@host:5432/db"
./nyro-server-linux-x86_64 --storage-backend postgres

# 或
export NYRO_MYSQL_DSN="mysql://user:pass@host:3306/db"
./nyro-server-linux-x86_64 --storage-backend mysql
```

### 多副本生产部署

在负载均衡器后运行多个 `nyro-server` 副本时，所有副本**必须**共享同一数据库和相同的 admin token：

| 要求 | 说明 |
|---|---|
| 共享数据库 | 使用 `--storage-backend postgres` 或 `mysql`，所有副本指向同一 DSN。SQLite 不支持共享。 |
| 统一 admin token | 每个副本设置相同的 `--admin-token` / `NYRO_ADMIN_TOKEN`。 |
| 配置同步 | 副本通过 `--config-poll-interval`（默认 3 秒）轮询共享 DB 的配置变更，路由/模型/Provider 的修改在一个轮询周期内传播。 |
| OAuth 交互式流程 | OAuth 授权回调必须路由到**发起该 session 的同一副本**。请在负载均衡器的管理端口（`19531`）上配置 sticky session（会话亲和）。 |

**健康探针：**

| 端点 | 端口 | 用途 |
|---|---|---|
| `GET /healthz` | proxy + admin | Liveness — 固定返回 `200` |
| `GET /readyz` | proxy + admin | Readiness — DB 可达返回 `200`，否则返回 `503` |

**关键环境变量：**

```bash
NYRO_MODE=all                      # all（默认）| proxy | admin
NYRO_ADMIN_TOKEN=<secret>          # 当 admin host 不是 loopback 时必须设置
NYRO_STORAGE_BACKEND=postgres      # postgres | mysql | sqlite（默认）
NYRO_POSTGRES_DSN=postgres://...   # backend=postgres 时必须设置
NYRO_MYSQL_DSN=mysql://...         # backend=mysql 时必须设置
NYRO_CONFIG_POLL_INTERVAL=3        # 配置 epoch 轮询间隔（秒，默认 3，0=禁用）
NYRO_WEBUI_DIR=/path/to/dist       # 从外部目录提供 WebUI（不填则使用嵌入资源；仅 admin/all 模式）
```

## 快速开始

**1. 添加 Provider**

进入 **Providers → New**，填写 Provider 的 Base URL 和 API Key。Nyro 会根据 URL 自动识别协议。

**2. 创建路由**

进入 **Routes → New**，设置虚拟模型名（例如 `gpt-4o`），选择 Provider 与目标模型。需要时可开启访问控制。

**3. 将客户端指向 Nyro**

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:19530/v1",
    api_key="your-proxy-key"  # 若关闭访问控制可用 "no-auth"
)

response = client.chat.completions.create(
    model="gpt-4o",  # 对应你配置的虚拟模型名
    messages=[{"role": "user", "content": "Hello"}]
)
```

**4. 同步你的 AI 工具（可选）**

进入 **Connect**，选择路由后点击 Claude Code、Codex、Gemini CLI 或 OpenCode 旁边的 **Sync**。Nyro 会自动写入正确配置。

---

## 开源协议

```
Copyright 2026 The Nyro Authors

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```

完整协议见 [LICENSE](LICENSE)。
