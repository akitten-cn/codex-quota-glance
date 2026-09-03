# 目标架构

```text
apps/codex-taskbar
  └─ 仅负责装配、生命周期和退出码

crates/domain
  └─ 额度、活动状态、任务栏布局等纯业务模型

crates/application
  └─ 数据源端口、状态仲裁与用例编排

crates/platform-windows
  └─ HWND、任务栏/副屏定位、DPI、Direct2D、托盘与 Explorer 恢复

crates/diagnostics
  └─ 结构化日志、滚动文件、等级热更新与脱敏导出

后续 adapters
  ├─ codex-app-server：官方 JSON-RPC/事件流
  ├─ codex-sqlite：内部数据库的只读兼容层
  └─ new-api：HTTP Provider 与费用数据
```

## 依赖方向

`domain` 不依赖 Win32、SQLite、HTTP 或 UI；`application` 只依赖 `domain`；平台和数据源适配器实现 `application` 定义的端口。最外层应用负责装配，禁止 UI 直接读取数据库或 Codex 文件。

## 数据源优先级

1. `account/rateLimits/read` 与 `account/rateLimits/updated`：5h/周额度。
2. `thread/tokenUsage/updated`：仅对本监视器 App Server 实例实际加载的线程提供实时累计 Token。
3. Codex SQLite：桌面端活动的共享只读探测；App Server 未提供有效 current-thread Token 时进行低优先级补位。禁止写入，禁止依赖不透明 JSON 正文。
4. `thread/status/changed`、`turn/*`、`item/*`：仅用于当前 App Server 连接自身可见的活动，不能假定它能旁听另一个 stdio 进程。
5. session JSONL：仅在 App Server 不支持所需能力时降级，并明确标记 `stale`。

## Codex CLI 定位边界

- 优先级为手工路径、当前用户 `%LOCALAPPDATA%/OpenAI/Codex/bin/<version>/codex.exe`、PATH。
- WindowsApps、非文件和错误文件名在执行前拒绝；候选必须通过有超时的 `app-server --help` 能力探测。
- App Server 使用显式 `app-server --stdio` 参数启动。完整路径只用于创建进程，不得进入 Debug、结构化日志或配置摘要。
- 定位失败不是应用启动失败：额度显示 Unknown，SQLite 只读活动/Token 后备继续工作。

## New API 运行时边界

- 账户、Key 配额/累计用量、今日日志是三个独立缓存域，各自保存当前健康状态和最近成功时间；单端点失败不得擦除其上次成功值，也不得把缓存继续标成实时。
- New API 设置热重载时先清空旧 Provider 身份缓存，再停止旧 worker，并以新的 generation 启动采集；旧 worker 的迟到结果必须被 generation 门禁丢弃。
- worker 的轮询等待可通过 stop channel 立即打断；若 WinHTTP 请求仍在阻塞，主线程不等待 join，迟到结果仍由 generation 隔离。
- 禁用或配置不完整时不启动 Provider worker。`codex_cli_path` 的变化仍在下次启动生效，避免运行中替换 App Server 会话造成账户和任务状态交叉。

## 任务栏布局约束

- 额度使用一个嵌套圆环组件：外环为周额度，内环为 5h；权威快照明确没有 5h 时只画外环。
- 状态灯始终使用柔和光晕。Idle/Completed 绿色呼吸 3 秒后停止动画并保留静态光晕；运行态持续呼吸。
- 支持 Left/Right 锚点。Right 以任务栏通知区域左边界为定位上限，不能以屏幕右边缘计算，否则会覆盖托盘、时钟和状态图标。
- 主副屏分别解析任务栏可用矩形，并按显示器设备 ID 保存锚点、宽度、偏移和预留间距。
- 与 TrafficMonitor 等第三方窗口的自动碰撞检测属于 best-effort；设置中必须保留手动偏移和预留宽度。

## 注释规则

- 每个模块使用 `//!` 说明职责、输入输出和禁止事项。
- 公共类型和函数使用 `///` 说明稳定语义。
- 仅为不明显的约束、Win32 生命周期和兼容性分支写代码块注释。
- 注释必须解释“为什么”，避免逐行复述实现。

## 日志字段

基础字段为 `timestamp`、`level`、`target`、`event`、`source`、`duration_ms`、`error_code` 和 `schema_version`。线程 ID 默认哈希；不记录认证信息、提示词、工具参数正文和完整用户路径。
