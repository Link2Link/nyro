# dsh-nyro-usage

DSH Web 的 [Nyro](https://github.com/Link2Link/nyro) provider 用量面板。

在 dsh Web GUI 侧边栏增加「Nyro 用量」入口，一屏展示 nyro 全部 provider
的上游用量：coding-plan 配额窗口（5小时 / 每周 / 每月）渲染为色阶进度条 +
重置倒计时，按量付费余额，今日/本月花销，以及运行时调度状态（可调度 /
配额耗尽）。

支持同时接入多个 nyro 网关：每个网关在面板中单独成组（可折叠、可单独刷新），
各网关分别请求，某个网关不可达或鉴权失败只在它自己的分组里报错，不影响其他网关。

卡片可自由拖动排序（每张卡片带 ⠿ 拖拽把手；拖到目标卡片左右两侧插入，
拖到网格空白处则排到末尾）。自定义顺序按浏览器、按网关存在 localStorage
（主网关为 `dsh.nyroUsage.cardOrder.v1`，其他网关为
`dsh.nyroUsage.cardOrder.v1:<网关 id>`）；新增 provider 追加在已排序卡片之后，
分组标题栏的「恢复默认排序」一键还原该网关的原始顺序。

数据链路：浏览器 → dsh webserver 同源路由 → nyro Admin API。nyro 无需开
CORS，admin token 不出 host 进程。

## Claude 账号池

Claude Pro/Max 账号池的 5 小时和每周配额是**加权估算**，使用与其他供应商一致的
「周期 / 进度条 / 百分比 / 倒计时」紧凑行。点击「账号池 · 加权估算」可展开查看
有效账号数、剩余配额、完整重置时间及说明；默认折叠，保持紧凑。同一行的卡片
保持等高，查询时间统一靠底部；展开详情时也保持该行对齐。

倒计时后的 `*` 表示仅部分账号重置，不代表整个池恢复；账号池不显示单账号的匀速
参考标记。数据不完整或过期时仍明确显示「未知」，不会当作 0%；重置时间已过时
显示「待刷新」。汇总只用于展示，不是调度依据。

优先读取匿名的 `usage.pool_quota`，兼容直连 Admin 响应的
`usage.account_pool.quota`。转发用量必须由上游 Nyro 提供 `pool_quota`，不透传账号
身份或配置。旧版本转发缺少 Claude 摘要时会提示升级上游，不会误判成金额未开放。

## 配置

设置 → 插件 → **Nyro 用量**：

| 字段 | 含义 |
|---|---|
| `enabled` | 插件总开关（默认开）；关闭后面板显示“已关闭”，不再请求网关 |
| `name` | 主网关的显示名称（可选，缺省用地址的主机名） |
| `baseUrl` | 主网关的 nyro 管理面地址，如 `http://192.168.31.2:19531`（结尾 `/api/v1` 可省略）；留空表示不使用主网关 |
| `adminToken` | 主网关的 `NYRO_ADMIN_TOKEN`（Bearer 鉴权）；secret 字段，回读脱敏 |
| `instances` | 其他 nyro 网关列表，每项 `{ name, baseUrl, adminToken, enabled }`，在面板中排在主网关之后 |
| `refreshSeconds` | 面板自动刷新间隔（默认 300，最小 15） |
| `cacheSeconds` | host 侧用量缓存 TTL（默认 30，按网关分别缓存；nyro 每次查询都实时请求上游，缓存避免频繁刷新打爆上游；0 关闭） |

原有的单网关配置（只填 `baseUrl` / `adminToken`）无需改动，会作为主网关继续工作。

配置卡中「其他 nyro 网关」一栏可添加、编辑、删除网关，也可单独停用某个网关。
已保存的 token 不会回显；编辑时 token 留空表示保持原值，新增网关必须填写 token。
直接写 profile 配置也可以：

```yaml
- id: nyro-usage
  name: dsh-nyro-usage
  config:
    baseUrl: http://192.168.31.2:19531
    adminToken: <主网关 token>
    instances:
      - name: 办公室
        baseUrl: http://10.0.0.8:19531
        adminToken: <token>
      - name: 备用
        baseUrl: http://10.0.0.9:19531
        adminToken: <token>
        enabled: false
```

网关 id 由显示名称生成（如 `办公室`、`备用`、`office`），重名时自动加 `-2`、`-3` 后缀；
主网关固定为 `default`。修改网关名称会改变 id，该网关的自定义卡片顺序随之重置。

配置卡内含「测试连接」按钮，通过 host 代理逐个验证已保存的网关，多个网关时分别列出结果。

## 安装（dsh 规范）

本地包（link）：

```bash
dsh plugin --profile web add link:/home/ubuntu/code/dsh-nyro-usage
```

或用 super-injector 热装配（同样的持久状态，免重启）：

```
dev_install_package(dir="/home/ubuntu/code/dsh-nyro-usage", profile="web")
```

先构建：`npm install && npm run build`。

## 路由

全部仅限回环（LAN 暴露的 dsh web 不服务）：

- `GET /api/nyro-usage/usage?instance=<id>&refresh=1` — 按网关返回 provider 用量
  `{ instances: [{ id, name, baseUrl, items, cachedAt, error, errorKind }] }`；
  不带 `instance` 时并发查询全部网关；`refresh=1` 绕过缓存
- `GET /api/nyro-usage/status` — 脱敏配置：网关列表、各网关缓存时间、各配置项是否已有 token（不含 token 本身）
- `POST /api/nyro-usage/test?instance=<id>` — 连通性与鉴权测试；不带 `instance` 时测试全部网关

## 开发

```bash
npm install
npm test        # host 解析/路由与网关列表编辑器的单元测试（node --test）
npm run build   # tsc + tsdown，输出 lib/index.js 与 lib/client.js
```

## 依赖

- nyro ≥ 提供 `GET /api/v1/providers/usage` 的版本
- dsh web profile 已装 `@linxin666/dsh-client-ui-web-ui-settings`
  （可选；配置卡缺省回退官方 settings scope）

## 许可

Apache-2.0。配置卡外壳与 staged form 内联自 dsh-web-ui 家族共享切片
（Apache-2.0）。
