# 官方登录详情卡片：竞品研究与本项目设计

> 调研日期：2026-08-22。本文只记录能够在源码或官方文档中核对的行为；第三方项目的私有接口实现不作为本项目的安全基线。

## 1. 结论

官方登录详情不能再是一张“所有数字同属一份实时数据”的表格。当前卡片组合了四条不同时间线：

1. `account/read`：账户身份与登录要求；
2. `account/rateLimits/read`：5h、Weekly、Credits 与消费控制；
3. `account/usage/read`：账户级聚合 Token 活动；
4. `thread/tokenUsage/updated` / SQLite：本机当前线程或降级记录。

前三项虽然都来自 Codex App Server，但能够独立成功、失败或不受当前版本支持；第四项更不能与账户累计相加。因此本项目采用“统一卡片、分区来源、独立 freshness”的方案。

## 2. GitHub 项目观察

| 项目 | 主要展示 | 数据来源与存储 | 本项目吸收点 |
|---|---|---|---|
| [CodexBar](https://github.com/steipete/CodexBar/blob/main/docs/codex.md) | 5h/Weekly、reset、Credits、账户与套餐、本地成本历史 | OAuth/API、App Server fallback、可选 Web extras、本地日志分层；缓存绑定账户与环境 | 远端额度、本地历史、估算成本不混算；显示来源；切换账户时隔离旧缓存 |
| [Quota Monitor](https://github.com/timmyagentic/quota-monitor) | 实时窗口、重置、pace/reserve、Reset Credits、7/30 天统计 | 实时额度与 SQLite 历史分开 | Popup 只显示快速判断；复杂历史留给完整 Dashboard；缺失窗口动态收缩 |
| [AIQuota](https://github.com/niederme/ai-quota) | 双弧额度、reset、plan、Credits、告警状态 | Swift 原生菜单栏；共享设置保存轻量快照 | 任务栏与详情卡保持同一额度语义；Credits 不与订阅窗口合并 |
| [ClaudeBar](https://github.com/tddworks/ClaudeBar) | quota grid、daily usage、cost、health、cache hit、pace | Provider 探针和独立刷新周期；JSON 设置；本地会话分析 | quota、Token、cost、health 组件化；无数据明确显示；不按 primary/secondary 位置猜窗口 |
| [TokenBar](https://github.com/Nanako0129/TokenBar) | 多 Provider quota、历史、账户隔离与凭据刷新状态 | 原生 Swift + Rust 核心；身份绑定缓存 | 缓存必须带来源、账户与 schema 身份；stale 不继续冒充实时 headroom |
| [WhereMyTokens](https://github.com/jeongwookie/WhereMyTokens) | 今日/累计 Token、成本、调用、模型分布、quota pace、来源健康 | Electron；来源归属 SQLite 与覆盖范围状态 | 借鉴来源/覆盖范围表达，不采用 Electron 作为本项目框架 |

共同模式：

- 最关键的额度窗口置于顶部，不把所有统计平均铺开；
- `Live / Cached / Error / Unsupported` 是数据状态，不是额度危险程度；二者使用不同视觉编码；
- 只有 Provider 确实提供的窗口才显示，不用 `0%`、`Unlimited` 或旧缓存补空位；
- 本地 Token、远端账户统计、估算成本保持 provenance；
- Popup 负责快速判断，历史趋势和高级筛选应进入后续完整页面。

## 3. 官方字段映射

官方契约以 [OpenAI Codex App Server 文档](https://developers.openai.com/codex/app-server) 为准。

| 卡片区域 | 字段 | 来源 | 是否落盘 |
|---|---|---|---|
| 身份 | 账户类型、掩码邮箱、套餐、认证要求 | `account/read` | 当前仅内存 |
| 额度 | 5h、Weekly、重置时间、额度名称 | `account/rateLimits/read` | 当前仅内存 |
| 附加额度 | Credits、个人消费上限、消费控制、Reset Credits | `account/rateLimits/read` | 当前仅内存 |
| 官方活动 | Lifetime、最近日桶、单日峰值、当前/最长 streak、最长 Turn | `account/usage/read` | 当前仅内存 |
| 本机线程 | 输入、缓存读、缓存写、输出、推理、累计、上一 Turn、上下文窗口上限 | `thread/tokenUsage/updated` | 当前线程仅内存；SQLite 只作显式后备 |

注意：官方公开文档没有稳定展开 `thread/tokenUsage/updated` 的完整 payload。本项目对 `last`、`cacheWriteInputTokens`、`modelContextWindow` 的支持属于当前 Codex 本机协议兼容层；UI 只展示观察到的绝对值，不承诺长期稳定字段，也不根据上下文窗口上限推算虚假的占用率。

## 4. 新卡片信息层级

```text
Header
  Codex 官方账户 / 登录模式 / 活动状态 / 最近额度更新时间

Quota cards
  5 小时：剩余百分比 + 重置时间
  Weekly：剩余百分比 + 重置时间
  缺失的窗口直接折叠

Left column — 账户与附加额度
  掩码身份、方案/认证、Credits、Reset Credits、消费上限与限制原因

Right column — Token 活动
  官方账户活动
  ─ 本机当前线程 ─
  线程累计、输入、缓存读/写、输出/推理、上一 Turn、上下文窗口上限

Health bar
  账户 / 额度 / 账户活动 / 本机线程，各自显示实时、缓存、不可用或后备来源

Footer
  明确声明账户活动与本机线程不相加
```

视觉规则：

- 额度充足使用低饱和绿色，警告与危险只由真实剩余百分比驱动；
- Cached 改为中性色，并保留最后成功时间；
- 无缓存时显示 `--`，不画红色 0%；
- 两个主窗口都存在时只显示两张等宽额度卡，不重复增加“最紧窗口”卡；
- 官方账户活动与本机线程之间有显式分隔标题；
- 健康条使用短文案，避免为了容纳时间戳把字号压得过小。

## 5. Freshness 与账户切换

- account、quota、usage 三个端点分别保存 `freshness + last_success`；
- 一轮刷新收齐后只发布一次聚合快照，避免依次出现“新账户 + 旧额度”等中间态；
- 任一端点失败只降级该区域，其他成功区域继续保持 Live；
- thread/turn/未知通知不能刷新 account、quota、usage 的成功时间；
- 收到 `account/updated` 时先清理旧账户范围内的身份、额度和账户 usage，再读取完整快照；
- UI 排序时间与“最后成功时间”分开，状态降级不能被旧时间戳拒绝。

## 6. 存储边界

当前版本官方快照只在内存中保存，不宣称具备重启后的持久缓存。

后续若增加磁盘缓存，允许保存：

- 掩码账户摘要；
- 归一化额度与账户聚合统计；
- endpoint 最后成功时间和 schema version。

缓存命名空间至少包含：

```text
mode + CODEX_HOME hash + auth kind + account fingerprint + schema version
```

禁止保存：邮箱原文、access/refresh token、API key、prompt/reasoning、工具参数、原始 App Server JSON、完整 threadId。线程历史如需标识，只能保存带本机随机盐的不可逆标识。

## 7. 不采用的设计

- 不读取 `auth.json` 后直连私有 ChatGPT endpoint 作为默认路线；
- 不把 API 等价成本称作订阅账单；
- 不把 `primary` 固定叫作 5h、`secondary` 固定叫作 Weekly；
- 不用 WebView2/Electron 实现常驻任务栏与详情卡片；
- 不在当前 Popup 中加入完整趋势图、模型筛选和多账户表格。

## 8. New API 详情卡补充约束

New API 的账户余额、Key 累计 Token、今日日志和 Codex 本地状态是四条独立时间线：

- 顶部“今日 Token”和构成图只使用 `/api/log/self` 的自然日聚合，不混入 Key 累计；
- 快览输入/缓存/输出/命中使用 `/api/usage/token/` 的 Key 累计口径，第五项为可用额度，不称为“本次”；
- 可用额度优先使用账户余额，账户端点没有余额时回退到当前 Key 的 available/unlimited；
- 账户、Key、今日日志健康状态必须逐项展示；任一端点成功不能把整张卡片标成“全部实时”；
- `truncated=true` 时所有今日汇总都标注“部分数据”，不能只在状态文字中隐含。
