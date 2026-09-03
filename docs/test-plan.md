# 测试计划

## 1. 测试层次

### 1.1 领域单元测试

- 额度 Present/Absent/Unknown 状态转换。
- weekly-only 时旧 session 不能复活 5h。
- 5h 恢复后内环重新可见。
- 多线程活动聚合优先级。
- Idle/Completed 仅呼吸 3 秒并保留光晕。
- 显示项隐藏、稳定排序和空间折叠。

### 1.2 协议契约测试

全部使用脱敏 JSON fixture，不启动真实 Codex：

- primary/secondary 为正常 5h+weekly。
- 只有 weekly、窗口顺序颠倒、缺少 duration、未知新窗口。
- `account/rateLimits/updated` sparse 更新不错误清空旧字段。
- thread/turn/item 生命周期和审批请求映射。
- 未知字段和新事件向前兼容。
- JSON-RPC error、超时、进程 EOF 和乱序响应。
- 当前 Schema 的 `tokenUsage.total`、`activeFlags=waitingOnApproval` 字段映射。
- `account/read` 完整读取会清除旧身份，邮箱进入领域层前完成掩码。
- `rateLimitsByLimitId.codex` 优先于 legacy `rateLimits`，未知桶不会覆盖 Codex 圆环。
- Credits、individualLimit、spendControlReached、reset credits 独立解析。
- `account/usage/read` 与 `thread/tokenUsage/updated` 不互相覆盖或相加。
- 账户、额度、账户活动三个端点分别维护最后成功时间；任一端点成功不能刷新其他端点。
- thread/turn/未知通知不得刷新账户、额度或账户活动的新鲜度。
- JSON-RPC 成功外壳中的无效 payload 不得标成 Live；保留旧值时必须降级为 Cached。
- 当前线程 Schema 分开保存累计 `total`、上一轮 `last`、`cacheWriteInputTokens` 与 `modelContextWindow`。
- daily buckets 不可用时显示 `--`；成功空数组与端点不可用语义不同。
- CLI 定位优先级、LocalAppData 版本升级、WindowsApps 拒绝、能力探测失败回退和日志脱敏。

### 1.3 Windows 组件测试

- 任务栏窗口枚举与显示器映射。
- Left/Right 纯坐标布局。
- Right 永不超过通知区域左边界。
- reserved offset、edge gap、宽度不足与碰撞降级。
- DPI 坐标换算和建议矩形处理。
- Explorer 重启后的状态机。
- layered DIB 未绘制像素 alpha=0，窗口不进入全局 TOPMOST。

### 1.4 集成与人工测试

任务栏嵌入会改变桌面外壳窗口层级；自动化验证坐标和渲染模型，最终透明度、抗锯齿边缘和 Z-order 必须在真实 Explorer 上人工确认。

实机数据链路还需验证：LocalAppData CLI 能完成 initialize/initialized，随后成功读取 account 与 rate limits；验证输出只记录窗口是否存在，不记录账户、额度数值或用户路径。

## 2. Windows 兼容矩阵

| 维度 | 场景 |
|---|---|
| OS | Windows 10 22H2；Windows 11 当前稳定版 |
| 显示器 | 单屏；主+副屏；拔插副屏；更换主屏 |
| DPI | 100%、125%、150%、200%；混合 DPI |
| 任务栏 | 主屏/副屏；自动隐藏；左对齐/居中图标；浅色/深色 |
| 锚点 | Left、Right；不同宽度、间距和偏移 |
| 共存 | TrafficMonitor 左/右不同位置；任务栏图标接近占满 |
| 生命周期 | Explorer 重启；锁屏恢复；休眠恢复；显示设置变化 |
| 权限 | 普通用户；Codex/Explorer 权限不一致 |

## 3. 关键验收用例

### Q-001：5h 不得复活

```text
旧快照：5h + weekly
→ 权威快照：weekly-only
→ RPC 暂时失败
→ session 仍含旧 5h
期望：内环始终隐藏；详情只能把旧值标为 last-known
```

### Q-002：5h 正常恢复

```text
Absent → 新权威快照重新包含 300 分钟窗口
期望：内环恢复，角度来自同一原子快照
```

### UI-001：右侧安全边界

```text
给定 taskbar rect、tray rect、widget width、gap、offset
期望：widget.right <= tray.left - gap；无法获得 tray rect 时返回 SafeFallback
```

### UI-002：灯动画终止

```text
进入 Idle/Completed 3 秒后
期望：动画 scheduler 无活跃 timer；静态光晕仍可绘制
```

### UI-003：紧凑布局

```text
给定 surface=320x40 DIP
期望：状态灯宽度为 16 DIP；主圆环宽高约 30 DIP；圆环垂直居中并为右侧摘要预留空间
```

### UI-004：中心百分比

```text
给定 Weekly 为 fresh 且存在 remaining percent
期望：圆环中心显示 xx%；Weekly unknown 时显示 --
```

### UI-005：点击详情

```text
左键或右键点击任务栏组件
期望：弹出约 880×590 DIP 浅色宽幅详情；顶部为动态额度卡及进度条，中部为互斥 Token 构成图，下方为账户/Token 分区与数据健康条；正文和图表标注在 100% DPI 下清晰可读，内容来自当前 NativeHostDetails 结构化字段
```

### UI-005A：官方登录详情卡片 V2

- 账户、额度、账户活动三个 RPC 独立标记 Live/Cached/Unavailable；任一成功不得把另外两个旧值标成 Live。
- 两个主额度窗口存在时显示两张等宽卡；5h 明确 absent 时只显示 Weekly，不能保留空卡或旧 5h。
- 官方账户活动与 Codex 当前任务有显式分区，二者不相加。
- 官方详情卡左栏使用紧凑账户/额度布局，右栏优先呈现 Token 用量仪表盘；当前拿不到历史数据时不得伪造趋势线。
- 当前任务显示缓存读/写、上一 Turn 与上下文窗口上限；不得把缓存写入重复计入总量，也不得仅凭窗口上限计算占用率。
- last-known 任务值必须标成“上次”或缓存；Token 快览只读实时最近 Turn，缺失时显示 `--`，不得把任务累计冒充“本次”。
- 底部健康条至少包含账户、额度、账户活动、本机任务四项，并使用可读短文案。
- 在 100%–200% DPI 下不得越出目标屏幕工作区。

### UI-006：Token 快览滑块

```text
鼠标进入任务栏组件，或最近 Turn Token 变化后进入 WaitingForUser/Completed/Failed
期望：上方出现不抢焦点的深色窄条，官方模式展示最近 Turn 的输入、缓存、输出、命中、本次及官方费用估算；
重复 Token 通知只刷新同一倒计时，不重复创建窗口；约 4 秒后自动关闭；启动预热、重连首份、SQLite 后备和 New API 聚合不得自动弹出；
不得展示内部术语“推理/线程”，不得把任务累计冒充本次，也不得把官方估算描述为实际订阅账单
```

### UI-007：New API 独立预览与口径

```text
运行 --visual-preview-new-api-details / --visual-preview-new-api-strip
期望：不读取 settings.json、DPAPI sidecar 或真实网络；只使用固定脱敏数据
```

- 详情卡顶部展示今日 Token 与可用额度，图表标题必须明确为“今日 API Token 构成”。
- 图表使用今日日志聚合，不得混入 Key 累计 Token；缓存输入不得重复计入普通输入。
- 快览前四项使用 Key 累计输入、缓存、输出和命中率；第五项显示“可用”，账户余额缺失时回退 Key available/unlimited。
- 当前数据源没有可靠单请求快照，禁止把 Key 累计或今日聚合标成“本次”。
- 健康条分别展示账户、Key、今日日志和 Codex 本地；部分端点成功时 Header 显示“API 部分实时”；全部失败但仍有上次成功值时显示“API 使用缓存”，各端点同时显示缓存年龄和当前失败原因。
- `truncated=true` 时今日统计明确标注“部分数据”。

### UI-008：高 DPI 与四向任务栏弹窗边界

```text
分别以 100% / 150% / 200% DPI，底部 / 顶部 / 左侧 / 右侧任务栏运行；
目标显示器包含主屏和负坐标副屏
期望：详情卡与 Token 快览全部落在当前显示器 rcWork 内；尺寸只等比缩小、不放大；
文字、图表和圆角卡片使用同一缩放因子，不允许窗口缩小后内容仍按原尺寸裁切
```

### UI-009：Explorer 重启与显示器切换

```text
详情卡或 Token 快览处于打开状态 → Explorer 重启，或设置切换 target_monitor_device
期望：任务栏主体使用重新发现的新 HWND 与新矩形挂接；旧浮层立即关闭；
下一次点击/悬停基于新显示器重新创建；托盘图标恢复；失败写入脱敏结构化日志
```

### OFF-001：官方账户身份切换

```text
旧 account/read=ChatGPT A → account/updated → 新 account/read=ChatGPT B
期望：旧邮箱、套餐、额度缓存立即隔离；UI 只显示掩码后的 B
```

### OFF-002：官方/本机 Token 分区

```text
account/usage/read 返回 lifetime=1M；thread/tokenUsage/updated 返回 total=20K
期望：卡片分别标注账户累计与本线程，不相加为 1.02M
```

### OFF-003：端点独立降级

```text
上轮 account、rateLimits、usage 均成功 → 本轮 account 与 usage 成功、rateLimits 超时
期望：账户=实时；账户活动=实时；额度=缓存并保留上次成功时间；整卡不得显示“全部官方实时”
```

### OFF-004：活动通知不得刷新官方数据

```text
官方额度上次成功时间=T1 → 收到 thread/tokenUsage/updated、turn/started 或未知通知
期望：当前线程可以更新；账户/额度/账户活动仍保持各自 T1，不得变成“刚刚更新”
```

### OFF-005：无效成功响应

```text
rateLimits/read 返回 JSON-RPC success 但 payload 无可识别额度；
account/usage/read 返回 summary=null
期望：对应端点不标 Live；已有值保留为 Cached，从未成功则显示 Unavailable
```

### OFF-006：线程 Token 当前 Schema

```text
thread/tokenUsage/updated 同时包含 total、last、cacheWriteInputTokens、modelContextWindow
期望：total=线程累计；last=最近 Turn；缓存写单独显示且不重复计入总量；
上下文窗口只显示容量，不在缺少可靠已用量时伪造占用率
```

### OFF-007：官方账户真实日桶趋势

```text
account/usage/read 返回乱序、重复和畸形 dailyUsageBuckets
期望：严格接受 YYYY-MM-DD 有效日期；同日采用最后一个有效值；按日期升序；
最多保留最近 90 日，详情卡只绘制最近 5 个有效点；少于 2 点时隐藏趋势区；
不得把缺失日期补成 0，不得用本地任务 Token 或视觉预览数据混入生产历史
```

### UI-010：官方详情卡纵向信息层级

```text
官方账户同时存在额度、账户活动、任务 Token 与至少两个日桶
期望：左侧额度与账户约占内容区四分之一，账户长值不与宽标签列横向挤压；
右侧顺序为总量、任务/账户明细、全宽趋势、Token 构成；任务与账户统计不相加；
不得为了复刻参考图而伪造估算价格、请求次数或按模型分布
```

### UI-011：趋势曲线与悬停

```text
给定至少两个真实 dailyUsageBuckets，并在 100%–200% DPI 下移动鼠标
期望：曲线通过每个真实点，逐段不超出两个端点值；悬停命中最近真实日桶；
Tooltip 显示日期和带千位分隔的完整 Token，长数值按 DirectWrite 实测宽度展开且不越出图表
```

### UI-012：托盘菜单与原生设置

```text
右键任务栏组件或托盘图标 → 依次执行显示详情、设置、重载、编辑 JSON、
打开配置目录、打开日志目录
期望：菜单选择通过 TPM_RETURNCMD 可靠回传；设置可编辑锚点、宽度、避让、
安全间距、目标显示器、减少动画、日志等级、显示项显隐与顺序；应用保持窗口，
保存仅在原子落盘和热应用成功后关闭；失败时保留草稿；未暴露的 New API 凭据和
CLI 路径不被覆盖。后台启动也必须让设置页可靠出现在前台，随后恢复普通 Z-order
```

### UI-014：设置页显示器、分页与小屏

```text
从目标任务栏打开设置 → 切换“任务栏/数据源”页 → 在负坐标副屏与 100%–200% DPI 重复
期望：窗口优先位于调用宿主所在显示器并完整落在 rcWork；两个页面 HWND 集合互斥，切页后旧页面无残影；
700–900 px 客户区不出现横向滚动，小屏只出现纵向滚动；底部保存/应用/取消始终可访问；
数据源页不得回显已有凭据，Base URL origin 变化必须要求明确替换或清除
```

### OFF-008：官方线程费用估算

```text
account/tokenUsage/read 返回 threadUsage.estimatedUsageUsdMicros 和 groups[]
期望：当前/最近线程显示服务端金额并标注“官方估算 · 非账单”；USD 缺失保持 --；
不得按未知模型猜价，不得把 credits 或订阅额度当作美元，也不得将 New API 手动倍率混入官方账户
```

### UI-013：后台子进程不得弹出控制台

```text
从托盘打开设置，同时启动 Codex CLI 能力探测和长期 app-server
期望：能力探测与 app-server 均使用 CREATE_NO_WINDOW；桌面不出现黑色控制台；
设置窗口可见并可交互，关闭设置后 app-server 继续后台运行
```

### 3.1 2026-08-23 当前实机结果

- Release 详情卡已在双屏任务栏实机显示，平滑面积曲线与任务栏组件同时可见。
- 原生设置页可被 UI Automation 完整枚举；任务栏/数据源页 Release 实机切换无控件重叠，正常宽度无横向滚动；“减少动画”勾选和“应用”点击成功。
- 临时 `settings.json` 原子写入 `reduce_motion: true`，日志出现 `settings_reloaded` 与 `settings_ui_applied`。
- `CREATE_NO_WINDOW` 修复后不再出现 Codex App Server 黑色控制台；设置页经短暂 Z-order 提升后可靠可见。
- 托盘菜单命令映射有单元测试；右键菜单全部条目的实机逐项点击仍需保留截图/日志证据。

### ACT-001：等待用户优先

```text
线程 A Executing + 线程 B waitingOnApproval
期望：任务栏灯为 WaitingForUser
```

## 4. 性能测试

- 冷启动/热启动各 10 次，记录中位数与 P95。
- 静态空闲 1/15/60 分钟的 Working Set、Private Bytes、CPU 和句柄数。
- 连续呼吸 10 分钟的 CPU、重绘次数和内存变化。
- App Server 关闭/启动/持续订阅三种模式分别测量。
- 8 小时运行、Explorer 重启 20 次、显示器拔插 20 次，无句柄和内存持续增长。
- Release EXE、Portable ZIP、安装包及首次运行数据目录分别记录体积。

### 4.1 当前 Release 基线（2026-08-23）

在 Windows 双屏开发机上使用软件 Direct2D 路径和
`target/release/codex-taskbar.exe --visual-preview-idle` 采样：

- 当前 Release 单文件约 2.21 MiB，Portable ZIP 约 1.20 MiB；
- 初始化完成后的约 14 分钟静态区间，Working Set 约 27.8–28.6 MiB；
- Private Bytes 大部分约 13.8–15.5 MiB；
- normalized CPU 接近 0%，GDI=13、USER=6，区间内无增长；
- 原始数据：`artifacts/performance/runtime-visual-preview-idle-20260823-103230.csv`。

首分钟包含 DirectWrite 字体、托盘图标和工作线程初始化，不能用首末端点增长率直接宣称泄漏。正式模式还会启动独立 Codex App Server 子进程，必须与 UI 主进程分开记录。60 分钟、Explorer 重启和反复打开/关闭浮层仍需继续验收。

## 5. 日志与隐私测试

- 每个日志等级过滤正确，运行时修改立即生效。
- 日志滚动和异常退出不损坏现有文件。
- fixture 中放入模拟 API Key、提示词和用户路径，断言输出中不存在原文。
- 诊断导出只包含允许字段和哈希后的线程 ID。

## 6. CI 门禁

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- Release build
- 依赖许可证/漏洞扫描（进入 P1 后启用）
