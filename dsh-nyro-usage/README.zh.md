# dsh-nyro-usage

DSH Web 的 [Nyro](https://github.com/Link2Link/nyro) provider 用量面板。

在 dsh Web GUI 侧边栏增加「Nyro 用量」入口，一屏展示 nyro 全部 provider
的上游用量：coding-plan 配额窗口（5小时 / 每周 / 每月）渲染为色阶进度条 +
重置倒计时，按量付费余额，今日/本月花销，以及运行时调度状态（可调度 /
配额耗尽）。

卡片可自由拖动排序（每张卡片带 ⠿ 拖拽把手；拖到目标卡片左右两侧插入，
拖到网格空白处则排到末尾）。自定义顺序按浏览器存在 localStorage
（`dsh.nyroUsage.cardOrder.v1`）；新增 provider 追加在已排序卡片之后，
工具栏的「恢复默认排序」一键还原网关原始顺序。

数据链路：浏览器 → dsh webserver 同源路由 → nyro Admin API。nyro 无需开
CORS，admin token 不出 host 进程。

## 配置

设置 → 插件 → **Nyro 用量**：

| 字段 | 含义 |
|---|---|
| `baseUrl` | nyro 管理面地址，如 `http://192.168.31.2:19531`（结尾 `/api/v1` 可省略） |
| `adminToken` | nyro 的 `NYRO_ADMIN_TOKEN`（Bearer 鉴权）；secret 字段，回读脱敏 |
| `refreshSeconds` | 面板自动刷新间隔（默认 300，最小 15） |
| `cacheSeconds` | host 侧用量缓存 TTL（默认 30；nyro 每次查询都实时请求上游，缓存避免频繁刷新打爆上游；0 关闭） |

配置卡内含「测试连接」按钮，通过 host 代理验证已保存的配置。

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

- `GET /api/nyro-usage/usage?refresh=1` — 全量 provider 用量（TTL 缓存）
- `GET /api/nyro-usage/status` — 脱敏配置 + 缓存时间
- `POST /api/nyro-usage/test` — 连通性与鉴权测试

## 依赖

- nyro ≥ 提供 `GET /api/v1/providers/usage` 的版本
- dsh web profile 已装 `@linxin666/dsh-client-ui-web-ui-settings`
  （可选；配置卡缺省回退官方 settings scope）

## 许可

Apache-2.0。配置卡外壳与 staged form 内联自 dsh-web-ui 家族共享切片
（Apache-2.0）。
