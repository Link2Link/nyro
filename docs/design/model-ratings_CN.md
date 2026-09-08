# 供应商模型评分

[English](model-ratings.md)

## 目标

Nyro 为每个精确的 **`供应商 ID + 上游模型名`** 保存一个人工综合能力分。
分数为 **0–100 整数**，只表达管理员的相对能力评价，不是客观 IQ、评测结果、
成功概率或倍数尺度；不包含速度、价格、可用性。

首期提供逐项编辑、持久化、跨供应商比较、供应商复制和配置备份。
**本期不增加图表，也不让评分影响现有路由。** 后续绘图或路由必须单独处理未评分，
不能把缺省状态转换成某个数值。

## 评分状态与模型标识

| 状态 | 表示 | 含义 |
|---|---|---|
| 已评分 | `status: "rated"`，整数分数及时间 | 有已保存评价，0 分也是有效评分 |
| 未评分 | `status: "unrated"`，`score: null`，`updated_at: null` | 查询成功，但没有评分记录 |
| 未知/错误 | 请求失败，界面明确报错 | 无法确定评分，不能冒充未评分 |

数据库只保存已评分记录，清除就是删除该记录，状态由记录是否存在派生。
供应商以 ID 标识，而非名称或 vendor。多个路由引用同一供应商模型时共享评分；
删除或重建路由映射不会删除评分。

评分 API 精确保留模型名的大小写、首尾空格、Unicode、命名空间和斜杠，不做
trim、模糊匹配、别名归并或 Unicode 归一化。名称不可全空白、不可含 NUL，
上限 1024 UTF-8 字节；超限报错而非截断。上游目录到 Nyro 模型名的转换，仍由现有
供应商协议解析边界负责。

除身份外只记录分数和更新时间，不增加备注、多维能力、推理档位、置信度或历史。
普通保存（包括再次确认同一个分数）使用服务端 UTC RFC3339 毫秒时间；并发编辑
采用最后成功写入者生效。

## 管理界面

- **可用模型**页显示分数或明确的“未评分”，支持逐个编辑。
- **模型评分**页（`/model-ratings`）以平铺列表跨供应商管理，可搜索、按供应商/
  评分状态/分数范围筛选，按分数/名称/更新时间排序及分页。
- 列表为成功目录、已知路由目标、已保存评分三者的并集。禁用供应商和已不在目录中
  的评分仍可管理。
- 默认分数降序；无论升序或降序，未评分始终置后。分数范围只包含已评分记录；
  同分按稳定的身份顺序排列。
- 输入必须是整数，不取整、不夹到范围内。空输入不等于 0，也不等于清除；清除使用
  独立确认操作。
- 评分请求失败时显示错误与重试，不能展示虚假的未评分数量。目录请求失败时说明
  列表可能不完整，但不妨碍编辑已保存评分。
- 只有成功目录中未出现的模型才标记“目录缺失”；禁用或加载失败显示“目录未知”。

评分管理页通过 `require_catalog=true` 获取目录，明确报告网络、HTTP、JSON 或
目录结构错误，而不是使用旧逻辑静默返回空目录/静态兜底。其他目录消费者保留
原有行为。保存和清除评分本身不查询上游。

## 管理接口

所有接口沿用现有 Admin API 认证，不暴露到代理侧的公共模型元数据。
桌面 IPC 调用相同的核心服务。

| HTTP | 用途 |
|---|---|
| `GET /api/v1/provider-model-ratings` | 所有已保存评分，包括禁用和目录缺失记录 |
| `GET /api/v1/provider-model-ratings?provider_id=…` | 某供应商的已保存评分 |
| `GET /api/v1/providers/:id/model-rating?model=…` | 某组合的明确已评分/未评分状态 |
| `PUT /api/v1/providers/:id/model-rating?model=…` | 保存 `{ "score": 85 }` 并返回记录 |
| `DELETE /api/v1/providers/:id/model-rating?model=…` | 清除单项，返回 `{ "ok": true }` |

模型名作为查询参数需要 URL 编码，尤其 `/`、`+`、`#` 和空格。
列表、查询、保存沿用 `{ "data": ... }` 响应封装，例如：

```json
{
  "data": {
    "provider_id": "provider-id",
    "upstream_model": "vendor/model-x",
    "status": "unrated",
    "score": null,
    "updated_at": null
  }
}
```

已评分时 `status` 为 `rated`，`score` 是包含 0 在内的整数，时间非空。
PUT 拒绝缺省/null/字符串/小数/超范围评分，以及客户端设置更新时间等多余字段。
有效供应商下重复清除幂等成功。评分错误使用 JSON `{ "error": "..." }`，状态码
为 400 输入错误、404 供应商不存在、501 后端不支持、500 内部失败；存储错误
不能转换成空列表成功。

列表枚举的是**已保存评分**，不是全部未评分模型。未来消费者必须在评分读取成功后
与自己的候选模型集合关联，或查询单项明确状态。目录不可用时，无法统计尚未发现
的未评分模型。

## 生命周期与备份

| 事件 | 评分处理 |
|---|---|
| 目录刷新、临时下架、探测失败 | 保留分数和时间 |
| 禁用供应商、更换地址/账号/渠道 | 保留，由管理员按需重新评价 |
| 删除路由映射 | 保留 |
| 同一供应商下精确同名模型重新出现 | 复用原分 |
| 新名称或别名出现 | 不自动继承 |
| 清除评分 | 只删除指定组合 |
| 删除供应商 | 在事务中删除该供应商全部评分 |
| 复制供应商 | 分数与原时间复制到新 ID，之后各自独立 |
| 导出配置 | 在所属供应商下写入 `model_ratings`，包含禁用/目录缺失记录 |
| 导入新供应商 | 绑定新 ID，保留原评价时刻，格式统一为 UTC 毫秒 |
| 导入时供应商同名已存在 | 供应商及其评分整体跳过，不覆盖当前评分 |

导出是现有版本 2 的兼容字段扩展，无评分字段的旧备份仍可导入。旧版 Nyro 可能
忽略新字段，不能指望它完整保留评分。

导入修改配置前先校验全部评分、标识、时间与重复精确键。某供应商评分恢复失败时
回滚刚创建的供应商，但不全局回滚先前已成功导入的其他供应商；错误反馈说明
部分完成情况。复制评分失败同样报错并回滚新供应商，不返回无评分的“成功副本”。

## 架构与验证

`ProviderModelRatingStore` 独立于路由快照、后端权重和第三方模型能力目录。
SQLite、PostgreSQL、MySQL 均持久化评分；YAML/MemoryStorage 保持现有只读/
不支持边界，不把重启即丢失的操作伪装成持久化保存。

单独修改评分不改变 `config_epoch`，不重载路由缓存或刷新配额。两处 UI 共享
经过校验的评分查询，修改后失效缓存，评分刷新与目录刷新独立。
数据库结构与临时库生成参考 SQL 的方法见[数据库文档](../database/schema.md)。

针对性验证命令：

```bash
cargo test -p nyro-core --test provider_model_ratings
cargo test -p nyro-core --test storage_provider_model_ratings
cargo test -p nyro-server --no-default-features rating_
cargo check -p nyro-core
cargo check -p nyro-desktop
```

WebUI 在 `webui/` 下执行 lint/build。轻量 UI helper 测试不需要额外测试框架：

```bash
# 在 webui/ 下执行
test_dir=$(mktemp -d /tmp/nyro-rating-tests.XXXXXX)
./node_modules/.bin/tsc src/lib/model-ratings.test.ts --outDir "$test_dir" \
  --module commonjs --moduleResolution node --target es2020 \
  --esModuleInterop --skipLibCheck
node --test "$test_dir/model-ratings.test.js"
```

存储测试通过显式的临时库变量开启
PostgreSQL/MySQL 实测；跳过不等于该后端验证通过。`tests/webui/` 的 Node/CDP
冒烟测试只使用隔离的临时服务，验证真实浏览器操作，不连接现有部署。
