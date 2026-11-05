# 按模型区分的配额桶机制

状态：实施契约，2026-11-05。功能已在开发分支实现并进行离线验证；发布状态另行跟踪。无需 SQL schema 迁移。英文规范版本：[quota-buckets.md](quota-buckets.md)。

## 当前实现与证据边界

已实现分桶状态/准入、不可变 attempt 归属、保留日志的分桶价格和筛选、结构化管理卡片/映射编辑，以及兼容 DSH 的对外独立用量条目。Google 通用标签本身不能证明 Flash/Argon 归属：必须有精确官方关联，或对已发现资源显式验证绑定；仅发现模型/目录百分比不作为健康独立额度供应商发布。未绑定前可能显示未解析诊断，不生成原型中的虚构固定窗口两桶。

尚无经脱敏真实样本确认的“Google 原始拒绝字段 → summary constraint ID”映射。因此未知 429 使用带 generation fence 的窄模型冷却和刷新；内部可信约束拒绝 API 只接受已知强制 raw window/shared resource，并阻止旧刷新覆盖新拒绝，没有编造原始错误解析器。子日周期与容量仅使用正向识别的固定窗口，目录、rolling/未知窗口不能推算精确周期。

运行身份/fence/实时资源映射（`quota_bucket_owner.*`、`quota_bucket_fence.*`、`quota_buckets.*`）明确不导出配置备份，导入时也跳过。恢复后的供应商须重新发现并绑定，不能把旧账号额度身份带到新配置。永久日 Token 台账不变。桶详情使用单 SQL 元数据游标聚合；分页日志仍走兼容元数据后过滤路径，高并发下可进一步优化为 snapshot/keyset 保证一致性。

## Google 双模型族展示

用量视图只折叠已确认的 `gemini-3.8-flash` 与 `gemini-4-argon` 两个模型族。基础 ID 及精确的 `-extra-low`、`-low`、`-medium`、`-high`、`-tiered` 后缀归入对应卡片，Argon 的 low/medium/high 不再单独成卡。未知后缀、近似型号和其他模型不猜入两族；每个权益作用域至多显示两张已观察到的卡，缺少观察时保持未知，不补造 0% 或免费容量。

这是标记为 `google_family_display_v1` 的核心只读展示投影，不是新配额绑定。注册表、转发缓存、配置编辑器、强制 fence 和不可变 attempt 归属仍保留原始模型/资源证据。结构化管理响应在原始桶计价**之后**投影，原始 ID 保留为非卡片资源。合成模型族 ID 不能用于保留日志筛选，来源详情链接使用原始 bucket + scope。前端不自行归族或合价，模型和诊断信息默认折叠；非 Google 展示不变。

同窗口使用率取保守最大值，绝不求和；不同 reset 不借用某一来源的时间，族级 reset 保持未知。累计金额每个作用域来源分区只计一次，跨两族的混合来源仅按已有精确模型计价明细拆分。多来源或混合模型族无法证明同一统计周期时，本周期金额/容量保持未知；有效的单来源周期保留三项金额。供应商总计和未归属/历史记录不会复制到两张卡，也不按今日分组重分历史。

批量发布复用这两个模型族，其他原始诊断留在结构化视图；仅目录观察的族明确标为未知，不作为健康独立供应商。代理转发先按原始桶判断**获授权的精确路由**，最后只折叠可见输出；独立的路由准入状态、隐藏兄弟模型与管理金额脱敏不变。本次展示调整不声称新增超出原始证据的跨档位准入或耗尽联动。

## 一、目标与结论

保留一个供应商配置和凭据，在内部引入独立配额桶。以用户确认分别计量的 Gemini 3.8 Flash 和 Gemini 4 Argon 为首批场景，实现：

1. **分别展示**：各桶自己的额度窗口、已用比例、重置时间、观察来源及可调度状态。
2. **分别统计**：各桶的请求次数、输入/输出/缓存读取 Token、累计等价 API 价格、本周期价格和周期容量估算。
3. **分别调度**：请求只能使用实际目标模型适用的桶。Flash 耗尽不影响 Argon；同一桶的五小时和周额度必须同时满足。
4. **上下游一致**：VPS 按远端虚拟模型路由向阿里云透传对应可用状态，不能用 Argon 的余量掩盖 Flash 耗尽。

不为获得两根额度条而复制供应商；不自动把 Flash 请求换成 Argon；不自动开启付费超额；不在本期新增 Google 多账号池。等价 API 价格不是实际账单、套餐价格、额度单位或剩余余额。

## 二、现有代码的问题

- Google 解析器保留了 group、display name、bucket ID、window、剩余比例和 reset，但未保留可直接使用的“模型 → 官方桶”关联。
- `quota_summary_tiers` 把不同模型族按窗口合并，仅留下每个窗口中用得最多的值，丢失桶身份。
- 目录回退路径先合并档位，再取 Gemini 模型族的众数，最终只显示一个 `gemini` 桶；独立模型额度被隐藏。
- `google_scheduling_observation` 又取合并结果中用量最少的行代表整个供应商。这里把“独立模型桶是备选”与“同桶多个窗口须同时满足”混为一谈；周额度已满可能被健康五小时额度掩盖。
- 调度注册表只以 provider ID 为键。`usage` 策略给该供应商所有模型同一个分数；`subscription_pool` 这个合成名称又不属于标准窗口，导致 Google 无法使用正常的周/五小时节奏评分。
- 价格统计选择一个供应商周期，累计所有模型价格，再除以某一个已用比例；Flash 与 Argon 独立后，分子分母可能属于不同桶。
- UI 只有平铺 tiers 与一个价格摘要；Nyro 用量转发只有供应商级状态，缺少目标模型的桶约束。

因此，必须先保留桶身份，再统一驱动展示、统计与调度，不能仅修改前端。

## 三、领域模型与不变量

```text
供应商
└─ 额度所属身份 / 权威来源作用域
   ├─ Flash 桶：五小时约束 AND 周约束
   ├─ Argon 桶：五小时约束 AND 周约束
   └─ 可选的全局共享约束：明确同时作用于两个桶
```

### 1. 身份层级

- **Provider**：操作员配置的路由目的地。
- **Quota owner**：上游订阅/授权权益所属身份；内部由供应商、稳定账号或旧单凭据身份、权威来源和 project/entitlement 命名空间确定。
- **Quota bucket**：一组请求共同消耗的额度资源，例如 Flash 独立额度。
- **Quota window**：该桶上的一个时间约束，例如五小时或周窗口。
- **Quota binding**：实际模型到主要计量桶及所有适用约束的映射。

上游 `bucketId` 可能指的是某个窗口约束，而不是模型族父桶，适配器必须保留这两层关系。token 刷新不产生新额度；凭据 generation 用于阻止旧结果写回，不应导致历史桶身份不断变化。更换订阅身份/project 不能继承旧身份的额度状态。公开 ID 使用稳定不透明标识，不暴露邮箱、账号或 project。

### 2. 核心不变量

1. 单次请求按实际发往上游的模型与所属身份判断，不按客户端别名、价格归一模型或 UI 分组猜测。
2. 同一请求所有适用窗口/共享约束是 **AND**；仅明确配置的替代目标或备选账号是 **OR**。
3. 一个独立桶耗尽不能阻塞兄弟桶；账号授权不可用或共同全局额度耗尽可以阻塞多个桶。
4. 每个上游 attempt 只有一个主要统计归属桶，或者明确未归属；多个窗口及共享约束不能让 Token/价格在供应商总计中重复累计。
5. 不对不同桶/窗口的百分比求和或平均；未知不是 0% 已用或无限额度。
6. 查不到价格或统计出错不影响额度观察及调度，不以显示用的容量估算决定路由。

## 四、桶发现与模型绑定

推荐优先级：

1. 上游响应明确提供的模型、权益资源、强制额度约束关联。
2. 经真实响应证据验证、带版本和适用渠道/权益范围的供应商适配规则。
3. 操作员显式绑定发现的资源 ID 与精确模型选择器；显示来源、生效时间和版本，拒绝冲突的主要统计归属。
4. 无法确定时标记未解析，分别保留原始观察；不能伪造共享关系。

**严禁仅凭相同剩余比例、相同 reset、宽泛 `gemini-*` 前缀、displayName 子串或价格匹配器认定调度绑定共享。** Flash 档位有证据共享时可绑定；Argon 的 bare/tier 名称仍保留实际调用/准入语义。上方双模型族白名单仅折叠只读用量卡片，不作全局后缀剥除或改写原始绑定。

本文依据用户确认的 Flash/Argon 独立关系设计，未连接线上获取官方桶 ID，也不假装现有 DTO 已能自动证明模型绑定。实施前先取得脱敏目录/summary 样本：若官方响应只有通用标签，首版使用显式可审计绑定；未绑定的桶仍可展示，但不参加错误的模型级额度推算。

建议内部类型：

```text
QuotaScopeKey(provider, owner, authority)
QuotaBucket(id, scope, label, model_binding, window_refs, shared_constraint_refs)
QuotaWindow(id, kind, used_percent?, resets_at?, duration?, source, observed_at, quality)
QuotaBinding(primary_bucket?, applicable_constraint_refs[], source, version)
PreparedAdmission(client_model, backend_model, wire_model, owner, generation,
                  quota_binding, snapshot_revision)
QuotaEvaluation(eligible | exhausted | unknown, blockers[], reserve, freshness, score?)
```

模型绑定和观察来源必须独立：一个桶可以由人工确认模型归属，同时额度百分比仍来自官方强制 summary。

## 五、用量观察、缓存和恢复

- 强制额度 summary 和模型 discovery 独立查询、独立失败。目录失败不妨碍更新有效 summary；summary 失败不能用更乐观的目录值覆盖已确认耗尽。
- 每个 owner/constraint 保留来源、观察时间、质量和 last-good。Flash 刷新成功不代表 Argon 也刷新成功；部分响应、缺项不是删除或恢复。
- 原子发布并校验 owner generation、映射/snapshot revision 与 rejection revision。刷新开始后新出现的拒绝，不能被该旧刷新结果清除。
- 只有同作用域、足够权威的新观察证明所有适用阻塞条件恢复，才清除耗尽。reset 到点只是安排复查，不自动当成额度已满。
- 未知 429 的短暂限流到期是解除 throttle，不是确认额度恢复；已知强制耗尽不能退化成未知放行。
- 沿用每 owner 合并刷新、退避、正常五分钟/耗尽后更快复查。冷启动或未绑定目标仅在现有并发/健康限制下受控探测，不伪造优先级。
- 必要时通过既有 settings 存储保留紧凑的强制阻塞证据，避免重启等同于额度恢复；凭据/权益作用域变更有明确失效规则。

## 六、调度设计

### 1. 先准备实际目标，再做最终准入

共用一个准备结果：解析 wildcard、后端模型、账号/凭据、reasoning 覆盖、渠道别名以及最终 wire model，再找到适用桶。选择器可用缓存作初排，不额外发网络查询；账号绑定后重验 generation，真正发往上游前再次确认 hook/encoder 没有把实际模型换到未获准桶。

不能另写一个“猜测用的 quota model resolver”。编码、准入、反馈、日志使用同一身份结果。每次 retry/queued sibling 重新检查；不能让旧 snapshot 允许已耗尽目标继续排队发送。

`priority`、`weighted`、`latency`、`usage` 全部服从同一模型级准入。Flash 桶不可用时跳过 Flash 目标，尝试已配置的其他 Flash 供应商；只有用户明确配置跨模型 fallback 才能改用 Argon。

### 2. usage 策略的权重

每个桶内部延续现有节奏公式：

```text
r = 剩余额度比例 / 剩余时间比例
weight = clamp(r)^3 × 原有窗口易过期加成
```

保留“最大已识别主窗口负责节奏评分”的策略：月、周、日、五小时；但所有短长窗口都负责准入，不能只检查评分窗口。请求若同时受多个独立必需约束组约束，用最受限组限制有效节奏。时长未知的目录额度不假设成周/五小时。

**首版保持每个供应商一份外层抽签权重，避免重复目标放大份额：**

1. 对当前虚拟模型，按 provider，再按相同 quota-choice/适用约束签名分组。
2. 同桶多个别名/重复行只有一份内部桶权重；组内按原静态目标权重选模型行。
3. 不同桶独立按自身节奏选择与排序。
4. 供应商外层权重取可用内部组中最大的有效权重，**不求和**。可用独立桶能提高代表性优先级，但多配几行不会让供应商获得多张抽签票。
5. Included 优于 unknown，unknown 优于明确授权的 paid reserve；blocked 无权重。纯未知供应商沿用一份未知权重，未知桶不为已知桶虚增优势。

这是明确的首版兼容策略，不声称独立百分比可换算为可相加容量。以后若需要“容量越多供应商越多流量”，须另定义可比较的容量单位与显式策略。

### 3. 拒绝反馈与隔离

- 可信结构化额度拒绝：更新对应 owner/constraint；共享同一桶的档位一起受限，但 Argon 不跟随独立 Flash 被封。
- 未知 429：仅作用于本次 owner/model/endpoint 的短暂冷却，并合并刷新，不直接宣称供应商或整账号额度耗尽。
- RPM/拥塞与额度耗尽区分；授权失败封对应凭据/账号；网络、模型无权限沿用精确 scope 的健康规则。
- 不突破现有 deadline、重试总数、输出提交边界、账号/会话回放约束。
- 决策日志记录桶、约束、来源、评分窗口、snapshot revision 与精确跳过原因。不能因某模型熔断把同供应商其他模型也标记为跳过。

## 七、使用量与价格统计

### 1. attempt 当时记录归属

在已有 `route_decision` JSON 内新增服务端生成的类型化、版本化 `quota_attribution`，记录实际 attempt 的：

- provider/owner 的不透明作用域 ID；主要统计 bucket；附加约束引用；
- 后端模型与实际 wire model；绑定来源/版本；
- dispatch 时间和归属状态。

与 ordinary payload 开关无关，清正文不清归属。不信任客户端自报 bucket，不写凭据/邮箱/project。多窗口不得多次计价；同一客户端请求发生 retry 时按每个真实 attempt 记录实际用量，不因最终只有一次成功而抹掉已发生消耗。未联系上游的 skip/build failure 不能当成实际消耗额度的证据。

旧日志仅在“当时有效且唯一”的规则下归类并标为 inferred。重绑、换账号、未知模型或证据冲突留在 **Unassigned / 未归属**；不把今天的新规则反写成过去事实。已下线桶保留历史统计。

### 2. 一次扫描、多桶多周期

扩展既有 provider retained-log accumulator，一次读取该供应商日志，按 owner + 主要 bucket + 精确计价模型分组，同时应用每桶独立 `[start,end)` 周期并统一截止时间。复用批量价格快照/缓存；不为每个桶单独扫库，也不从对话正文解析归属。

新归属按 dispatch 时间定义周期，但只有截止时间前已被记录/可见的用量进入快照；旧日志 emission 时间只能算旧数据估计。跨 reset 长请求按 dispatch 规则归类，不能宣称与 Google 真实扣账时刻完全一致。缓冲、丢日志、网关外使用都会影响覆盖。

**保留现有语义：供应商卡片价格基于保留日志，删日志会减少累计值。** 永久 UTC-day Token 台账是另一套统计；它只有 provider/model/day，没有 owner/bucket/精确时间，不能用来还原任意 reset 周期。未来要永久桶账本，必须单独设计迁移、留存及去重，不在本次暗改累计口径。

### 3. 同桶同周期计价和投影

每桶优先已识别周窗口，无周窗口时才使用已识别五小时。周窗口存在但失效/冲突/过期时，不偷偷换短窗口。时长或 reset 未知则本周期/容量未知，不能猜自然周。

```text
本桶本周期 API 等价价格
  = 本桶该周期的已记录 Token 按对应模型参考价/人工价累计

本桶周期总容量估算
  = 本桶本周期 API 等价价格 / 本桶同周期已用比例
```

估算只用于新鲜、同 owner/bucket/window/source、周期一致且利用率 >0、有可靠已计价样本的观察。证据不足则不估算；覆盖不全时明确标记部分/低可信。已用 0% 可显示本周期已记录金额但不除零。极小舍入比例、非有限值、溢出不产生“巨大可用余额”。共享全局约束即便展示重叠视角，也不能与子桶容量相加。

逐模型复用现有 token pricing engine。Argon 缺价不能套用 Flash 价；币种分开，缺价不是免费；缺 Token 与确知 0 区分；cache read 已在 gross input 中，不重复加。统计补充 missing usage、cache coverage、未计价/推断/未归属等覆盖信息。

供应商总计按主要统计归属满足：

```text
供应商保留用量/价格 = 所有主要归属桶之和 + 未归属
```

不合并不同桶的“本周期”和“周期容量”。

## 八、API 与界面

共享 core Admin DTO 增加可选 `quota_buckets`、模型级 scheduling detail。精确供应商接口和显式 provider 视图维持真实供应商及嵌套桶，服务 Nyro 自有 Web/desktop 管理；**对外批量用量视图则把每桶发布为独立只读供应商条目**。旧单桶供应商维持原结构，不能把重复窗口平铺进一个条目。

每桶返回：稳定不透明 ID、English label、精确适用模型/别名、绑定来源状态版本、窗口数组、共享约束引用、观察质量/新鲜度、可调度状态、Admin-only 价格及覆盖信息。窗口使用自己的 ID，不把多个桶都塞成同名 `weekly_limit` 扁平行。

### 卡片草图（示意，非线上数据）

```text
Google AI Pro                         部分模型可用

Gemini Flash                         额度耗尽
  五小时    [██████████] 100%        重置 18:00
  每周      [██████░░░░]  60%        重置 周四
  保留请求 / Token：……
  累计 API 价格 $A   本周期 $B   周期容量 ≈ $C
  适用：gemini-3.8-flash 及已确认共享档位

Gemini Argon                         可调度
  五小时    [██░░░░░░░░]  20%        重置 20:00
  每周      [███░░░░░░░]  30%        重置 周六
  保留请求 / Token：……
  累计 API 价格 $D   本周期 $E   周期容量 ≈ $F

供应商保留日志总价格：$G    未归属：$H
```

供应商不再显示混合百分比；展示“可用 / 部分可用 / 耗尽 / 未知”。点桶打开用量详情，增加 **All / Flash / Argon / Unassigned** 筛选；概览、模型统计、分页请求列表必须使用同一后端归属条件，并可叠加精确模型筛选，不在浏览器只过滤当前页。

继续一个 provider 级 Query/cache，不每桶发一轮用量查询；React key 使用 bucket/window 复合身份。窗口、价格状态、stale 判断由后端提供统一含义，Web/desktop 不各自实现额度业务规则。规范默认文案英文，显式提供中文。

## 九、对外发布：每个桶表现为独立供应商

这是已确认的展示要求：Nyro 自有管理页保持“一供应商内两个子卡”；DSH 等外部用量消费者获得两个普通供应商条目：

```text
Google AI Pro Year · Flash
  5小时  100%    每周 60%    额度耗尽

Google AI Pro Year · Argon
  5小时   18%    每周 30%    可调度
```

这些是 **Published Usage Entry / 已发布用量条目**，不创建真实 Provider、不复制凭据、不增加路由目的地或额度容量。

### 已核对的 DSH 实际行为

当前装配插件来自 [dsh-nyro-usage](../../dsh-nyro-usage/src/nyro.ts)，不是旁边旧 checkout：

- host [直接请求 Admin 批量接口](../../dsh-nyro-usage/src/nyro.ts#L297-L303) `/api/v1/providers/usage`，且只检查 `data` 是数组。因此**仅修改 proxy 用量转发还不够**。
- [卡片展开器](../../dsh-nyro-usage/src/client/panel/upstream.ts#L41-L81) 用直接条目的 `provider_id`，或 relay provider 与远端 `id` 的组合，保存卡片和拖拽顺序；嵌套远端条目会替换 relay 汇总卡片。
- [额度条渲染](../../dsh-nyro-usage/src/client/panel/ProviderCard.tsx#L171-L175) 使用 `key={tier.name}`；窗口名用于翻译和节奏参考线。一个条目两个 `five_hour`/`weekly_limit` 不仅难以辨识，还会有重复 key。
- 现有 DSH 不解析 `quota_buckets` 或 `api_price`，增加这些嵌套字段本身不会自动变成两张卡，也不会自动显示金额。按桶平铺可复用旧卡片；金额若要在 DSH 显示，另需插件 UI 支持，不能假装已有。

### 接口视图和兼容约定

| 接口 | 返回约定 |
| --- | --- |
| `GET /api/v1/providers/usage`（默认，或 `?view=buckets`） | 维持 `{data:[...]}` 包装；多桶供应商展开成多个只读用量供应商行。现有 DSH 无需理解嵌套桶即可显示两张卡。 |
| `GET /api/v1/providers/usage?view=providers` | 保留真实 provider 行与嵌套 `quota_buckets`；Nyro Web 管理在同一版显式请求该视图。 |
| `GET /api/v1/providers/:id/usage` | 只接受真实 provider ID，返回结构化父供应商和桶。 |
| core `list_provider_usage` / Tauri command | 仍以真实供应商为单位；HTTP 外部投影在独立服务/辅助层完成，不改内部路由身份。 |
| `GET /api/v1/providers` / provider CRUD | 真实配置不变，不接受新的展示 ID 当配置或路由 ID。 |
| `GET /v1/nyro/usage` | `providers[]` 同样按桶展开，精确绑定已授权路由；增加版本/能力描述，仍保留旧客户端可读的扁平字段。 |

默认批量行数变化是**明确的枚举契约变更**，不是简单新增可选字段。依赖“一行等于一个真实 Provider”或拿该 ID 做 CRUD 的消费者必须迁移到 `view=providers`。发布时同步修改 Nyro Web 的 HTTP 映射；desktop 调 core 不受影响。拒绝未知 `view`，避免悄悄返回错误视图。

DSH 连接测试数来自真实 `/providers`，面板计数来自可展示卡片；展开后两者不同是正常的，后续插件文案应分别说明“配置供应商数”和“用量条目数”。

### 每行字段规则

1. **稳定唯一 ID**：以源实例、真实 provider、owner/bucket 身份生成持久不透明展示 ID；不以名称、数组下标、reset 或百分比生成。重置/改名/刷新 token 不漂移，更换权益身份才换 scope。Admin 行 `provider_id` 必须等于 `usage.provider_id`；proxy 行 `id` 同样与 `usage.provider_id` 一致。
2. **可追溯但只读**：可选类型化 `usage_entry` 元数据标明 `kind=quota_bucket`、源 provider 引用、bucket 引用。Admin 可关联真实配置；proxy 只能使用匿名安全引用，不含邮箱/project/账号。客户端不能按父引用或相同名称把桶重新合并。
3. **替换而非叠加**：多桶情况下用子桶条目替代父供应商汇总用量行，不能父行加两子行重复展示；不按思考档位或每个窗口继续拆供应商。未启用分桶的单桶旧供应商维持原 ID/名称；进入分桶模式后，即使仅剩一个桶可见，也不让现存桶 ID 来回变动。
4. **窗口名仍标准**：每行只含本桶 `five_hour → daily → weekly_limit → monthly`，每名称最多一条。不改成 `flash_weekly_limit` 等伪标准，否则会破坏旧插件本地化和节奏参考线。重复/冲突的官方约束不可覆盖成假精确值。
5. **共享约束不伪装额度供给**：共享/global 约束在安全 sidecar/阻塞详情中描述，不发布成独立普通供应商，也不塞成本桶第二根同名周额度条。每个桶的有效 scheduling 必须包含账号授权和所有共享约束，旧消费者即使不理解 sidecar 也能看到正确阻塞。
6. **状态字段不混用**：外层 `status` 仍为查询结果 `ok/error/unsupported`；额度耗尽写 `usage.scheduling`。`is_enabled` 继承真实操作员开关，不因耗尽变 false。局部刷新失败保留该桶 last-good/质量信息，不清兄弟桶、不补 0%、不伪 eligible；完全无法解析时仅返回明确未知/错误诊断，不为猜测的桶复制多份错误。
7. **价格和余额不重复**：Admin 展开行仅附本桶已归属价格，绝不把供应商总金额/账号池/共享余额/spends 复制到每个桶；未归属历史只留父供应商管理视图。proxy 继续剥离所有嵌套 Admin 价格和身份；币种不暗加、不转换。
8. **一次查询后投影**：缓存真实 provider snapshot，权限裁剪后在本地展开；不因两个显示条目查两次 Google。`view=buckets` 与 `view=providers` 的响应缓存/validator 必须区分，公用转发缓存不保存管理端价格。

英文规范中提供了可解析的 Admin JSON 示例：两行分别使用 `usage:demo-provider:flash-01` / `usage:demo-provider:argon-01` 示例 ID，`provider_name` 标出 Flash/Argon，`tiers` 各只有一对标准窗口，所有 reset 和状态各自独立。正式实现使用安全不透明 ID，不把示例 ID 的明文结构当成公开编码规范。

## 十、Nyro 串 Nyro 必须一起覆盖

实际拓扑是客户端 → 阿里云 Nyro → VPS Nyro → Google。阿里云配置的后端模型可能是 VPS 的虚拟模型别名，不能直接用 Flash/Argon wire 名反推 VPS 路由。

用量转发增加能力/版本和 **按调用者有权访问的远端虚拟模型/路由** 输出的：

- 精确路由对应的可调度状态；
- 匿名 bucket/constraint ID 与窗口、新鲜度；
- 该路由真正可用的 providers/targets 关系。

仅对“确实能服务该远端路由”的目标做 OR。每个发布桶的 `routes` 仅列其实际可服务且调用者有权限访问的远端路由；同一桶服务多个路由时只发一行，去重路由列表。收集路由时必须保留目标模型，不再像当前实现只收 provider ID。未知/冲突归属不能复制父 provider 的所有路由给每个桶，也不能因某桶可见就泄露同 provider 的其他隐藏桶。VPS 最终准入仍是权威，即使下游观察延迟也不能越过。

新结构使用明确 allowlist 的安全 DTO，并递归去除账号/project/邮箱、隐藏模型、所有嵌套 Admin API prices；不能沿用“顶层删一个 api_price”就认为安全。维持防递归、按 owner 合并刷新。阿里云本地日志金额和 VPS 日志金额是两跳视角，不能相加成实际 Google 消费。

只 bump report version 不足以保护旧 Nyro：当前解析器只检查 object/providers，旧调度按“所有行都耗尽”决定暂停。因此每个 primary bucket 的 effective scheduling 都必须包含授权及共享全局阻塞，不能发布一个健康共享约束行作为假备选。升级后的 peer 识别按路由 admission 能力；旧 peer 保留粗粒度摘要/未知，不猜精确模型桶关系。

## 十一、分阶段实施

1. **配额模型、绑定和观察**：core 新类型、Google 保留原始资源身份、获取脱敏真实样本、Flash/Argon 可验证绑定；去除新路径中的 modal/max/min 信息损失。
2. **调度正确性**：共用 prepared identity，scoped registry/gate/feedback/fencing，usage 内部分桶及单供应商外层份额，精确 trace。所有 balance 都验证。
3. **归属与统计**：attempt JSON 快照、一次扫描分桶、多周期计价/覆盖、未归属对账。首版复用现有 JSON/settings 可不改 SQL schema；如确需新表/索引，按仓库要求维护三后端、schema 文档及自动生成参考 SQL。
4. **UI/API 与双跳**：共享可选字段、双语子卡与筛选、匿名按路由转发、旧新实例兼容。不能以“VPS 页面能显示两个桶”作为整个功能完成。
5. **验收与发布另行执行**：单测、公开 Admin API、SQLite/Postgres/MySQL 聚合、前端渲染、离线两跳 HTTP 夹具；用户明确授权后最小真实查询/重测。设计阶段不发版、不部署、不指定必然进入的版本。

## 十二、验收用例

| 场景 | 必须行为 |
| --- | --- |
| Flash 五小时 100%，周 20%；Argon 健康 | 只挡 Flash，供应商部分可用 |
| Flash 五小时 10%，周 100% | Flash 被挡，绝不能取最少用量窗口放行 |
| 两桶都健康但 reset 不同 | 两套周期/价格/节奏，不生成公共周期 |
| 共享全局额度耗尽 | 两桶都被挡，显示共享阻塞原因 |
| low/high 已确认同桶 | 只一份桶权重，价格仍按正确模型 |
| 两模型同为 80% 但无绑定证据 | 不自动合桶 |
| summary 失败，旧 summary 耗尽、目录健康 | 保留强制阻塞和 stale 提示 |
| discovery 失败，summary 成功 | 发布有效额度，映射按旧证据或未知处理 |
| 部分返回、凭据旧 generation、晚到刷新 | 不清兄弟桶、不跨身份写回、不伪恢复 |
| Flash 未知 429 | 窄 scope 冷却/复查，不封 Argon |
| 同供应商重复目标、多桶 | 外层不增加抽签票；桶内独立评分 |
| 重绑/更换账号或 project | 历史身份不漂移；歧义旧记录未归属 |
| Argon 缺价、缺 Token、已用 0 | 不借价、不当免费、不除零 |
| 币种不同 | 分币种，不暗中汇率换算 |
| 清日志后仅永久日汇总还在 | 保留价格相应减少，不重建子日周期 |
| 阿里云请求 Flash、VPS 仅 Argon 有余额 | Flash 路由不被 Argon 余量掩盖 |
| 普通 proxy Key 查询用量 | 只见授权路由，嵌套无账号及 Admin 价格 |
| DSH 直接查询 Admin bulk，存在 Flash/Argon 两桶 | 恰好两张桶卡片，每张最多一根五小时和一根周额度条，无父汇总重复 |
| DSH 展开 relay 的 `upstream.providers[]` | 两张明确命名的远端桶卡替代 relay 摘要，每张 key 唯一 |
| 桶改名/reset/token 刷新、仅剩一桶可见 | 已发布 ID/拖拽顺序稳定，不跨实例碰撞 |
| Web/desktop 管理与 provider CRUD | 真实 provider 结构不变，展示 ID 不可写、不进入路由配置 |
| Admin bulk 默认与 `view=providers` | 明确选择视图，拒绝未知 view，响应缓存区分 |
| 共享额度/授权阻塞，旧 peer 读平铺行 | 每个适用 primary bucket 的有效状态都被阻塞，无健康共享行误掩盖 |
| 局部刷新失败/隐藏桶/一桶服务多个路由 | 不补 0、不泄露兄弟桶，同桶一行且路由精确去重 |
| 分桶金额存在未归属历史/共享余额 | 只发布本桶归属价格，不复制父金额，proxy 递归去掉 Admin 价格 |
| priority/weighted/latency | 同样尊重桶准入，不暗换模型 |

## 实施前必须验证的事实

具体 Google 上游模型到 constraint 的关联、Flash/Argon 各 reasoning 档位是否共享、窗口是固定周期还是滚动窗口，均要以实际响应或明确配置验证。只有 resetTime 不足以证明固定时长；无法确认时宁可显示未知/受控探测，不制造精确周期与容量。
