# 总体设计说明

## 1. 技术栈

- Rust 2024 + Cargo Workspace
- `windows-rs`：Win32/COM/WinRT 调用
- Direct2D / DirectWrite：任务栏和浮窗绘制
- Codex App Server stdio JSON-RPC：额度、活动、Token
- `tracing`：结构化日志
- Serde JSON：配置及协议

常驻路径不使用 Electron、WinUI 3、Tauri 或 WebView2。若未来设置/历史窗口确需 WebView2，只能按需创建，不得成为任务栏常驻依赖。

## 2. 模块与依赖

```text
codex-taskbar (composition root)
  ├─ application
  │    └─ domain
  ├─ adapters-codex-app-server
  │    ├─ application
  │    └─ domain
  ├─ settings
  │    └─ domain
  ├─ platform-windows
  │    └─ domain
  └─ diagnostics
```

规则：

- `domain` 不知道 HWND、JSON-RPC、SQLite、HTTP 或文件路径。
- `application` 定义用例和端口，不负责具体协议解析。
- adapter 把外部格式转换为领域模型。
- `platform-windows` 只消费语义化快照，禁止自行读取 Codex 文件。
- app 是唯一装配点，不承载业务规则。

## 3. 运行时进程

```text
codex-taskbar.exe
  ├─ UI thread：HWND、托盘、消息循环、Direct2D
  ├─ telemetry worker：状态仲裁、低频刷新
  ├─ log writer：非阻塞日志落盘
  └─ codex app-server（可选子进程）
       └─ stdio JSON-RPC 事件流
```

UI 线程只接收不可变 `MonitorSnapshot`。协议解析、磁盘和子进程 IO 不得阻塞 Windows 消息循环。

## 4. 数据仲裁

### 4.1 额度

App Server 成功响应被视为原子快照：

```text
Some(window) -> Present + Fresh
None         -> Absent + Fresh（tombstone）
RPC failure  -> Unknown + Stale，保留 last-known 仅供说明
```

session fallback 只能填充 Unknown，不能覆盖 Absent。不同来源或不同 observedAt 的窗口不得逐字段拼接。

### 4.2 活动

事件优先级：

```text
Failed > WaitingForUser > Executing > Reviewing > Thinking > Completed > Idle > Unknown
```

来源映射：

- `thread/status/changed.activeFlags` 含审批等待：WaitingForUser。
- `turn/started`：Thinking。
- command/file/tool item started：Executing。
- reasoning item：Thinking。
- review item：Reviewing。
- user-input/approval request：WaitingForUser。
- `turn/completed`：Completed、Failed 或 Idle 过渡。

聚合多个线程时，先保留每线程状态，再按优先级生成一个任务栏灯状态；详情页后续可以展示分线程状态。

## 5. 任务栏发现与布局

### 5.1 发现

- 主任务栏：`Shell_TrayWnd`。
- 副屏任务栏：枚举 `Shell_SecondaryTrayWnd`。
- 使用 `MonitorFromWindow`、显示器设备名和物理/逻辑矩形建立映射。
- 所有坐标先转换到目标窗口 DPI 上下文。

### 5.2 可用区域

```text
available_left  = 左侧系统组件右边界 + edge_gap
available_right = 通知区域左边界 - edge_gap
```

- Left：`x = available_left + reserved_offset`。
- Right：`x = available_right - widget_width - reserved_offset`。
- 任一边界无法可靠发现时，进入 SafeFallback，不使用屏幕边缘猜测。
- 枚举任务栏已有子窗口矩形进行 best-effort 碰撞检测；用户预留宽度始终高于自动推断。

### 5.3 生命周期

- 监听 `TaskbarCreated` 注册消息处理 Explorer 重启。
- 处理 `WM_DPICHANGED`、`WM_DISPLAYCHANGE`、`WM_SETTINGCHANGE`。
- 副屏断开时转移到主屏的安全浮窗/托盘，不保留越界 HWND。
- 默认将 HWND 挂接到目标 Explorer 任务栏；Explorer 重建后用新 HWND 重新挂接，失败不重试 busy loop。

## 6. Direct2D 绘制

- 任务栏宿主使用 `WS_EX_LAYERED` 和 32-bit premultiplied-alpha DIB，通过 `UpdateLayeredWindow` 合成。
- 每帧先清为 RGBA `(0,0,0,0)`；不绘制任何矩形背景，避免在深浅色任务栏上出现色块。
- 子窗口只位于任务栏 sibling Z-order，不设置全局 `HWND_TOPMOST`。

### 6.1 嵌套额度环

- 外环：Weekly；内环：5h。
- 从 12 点方向开始，顺时针绘制 remaining percent。
- 5h Absent 时不创建内环 geometry，外环使用单环视觉参数重新居中。
- Stale/Unknown 不画进度角；显示中性色边框或短横线。

### 6.2 光晕灯

- 核心圆使用实色 brush。
- 光晕使用多层低透明度圆或径向渐变，不使用大面积窗口模糊。
- 呼吸曲线使用平滑正弦/缓入缓出；只使光晕半径和透明度小幅变化。
- Idle/Completed 进入状态后运行 3 秒，随后取消动画定时器，保留静态光晕。
- 连续动画最高 20 FPS；仅 invalidate 灯的脏矩形。

## 7. 配置与日志

- 配置包含 `schema_version`，读取时规范化非法数值并保留向前迁移入口。
- 写入使用同目录临时文件、flush、原子替换，避免异常退出留下半个 JSON。
- 日志 Filter 支持运行时 reload；文件 writer 非阻塞。
- 所有数据源日志包含 `source`、`event`、`duration_ms`、`revision` 和错误码，不包含内容正文。

## 8. 故障降级

| 故障 | 行为 |
|---|---|
| App Server 不存在/版本过旧 | 显示 Unknown，尝试只读兼容源并明确 Stale |
| App Server 子进程退出 | 指数退避重启，不阻塞 UI |
| Explorer 重启 | 销毁旧父子关系，等待 TaskbarCreated 后重新发现 |
| 无法找到通知区域 | 禁止 Right 嵌入，回退浮窗/托盘并记录原因 |
| SQLite schema 变化 | 关闭该兼容 adapter，不扫描不透明正文 |
| Direct2D 设备丢失 | 释放 device resources，下一次 paint 重建 |
