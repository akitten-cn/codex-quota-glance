# Codex Taskbar：官方登录单一数据源重构设计

## 1. 重构原因

当前实现同时承担官方登录、New API、多个价格表、本地账本、两套设置窗口和多条状态后备路径。
这些职责相互覆盖，导致任务栏状态灯把低置信的 SQLite `inProgress` 一律显示为紫色“思考中”，设置窗口也因手写 Win32 自绘与滚动混用而出现残影、点击失效。

本次重构的目标是**停止叠加修补**，将产品收缩为一个可靠的 Windows 官方登录监视器。

## 2. 范围

### 保留

- Windows 任务栏透明悬浮条、详情卡片、托盘菜单与日志。
- 官方 Codex 登录的订阅额度、5 小时额度、weekly 额度与 Credits 条件展示。
- 只读访问 `%USERPROFILE%\\.codex\\thread_history_1.sqlite` 的活动状态后备。
- Direct2D 绘制的任务栏组件；该窗口常驻且不使用 WebView2。
- 可配置的显示器、左右停靠、宽度、显示项目、日志等级。

### 移除（仅 `codex-taskbar` Rust 工作区）

- New API 站点、API Key、User ID、Access Token、模型价格表、自动计费、本地账本和来源自动匹配。
- `adapters-new-api` crate 及其领域、设置、运行时、详情卡、测试和文档路径。
- 已验证不可交付的手写 Win32 设置页，以及尚未接入产品的 egui 实验窗口。

### 不在本次删除范围内

- `D:\\my_program\\api用量显示\\codex-quota-glance`
- `D:\\my_program\\api用量显示\\legacy`

它们属于旧项目/历史目录；除非用户另行确认，不作递归删除。

## 3. 目标架构

```text
官方 Codex app-server ──> OfficialQuotaService ──> MonitorState ──> Taskbar / Details
                                                       ^
Codex SQLite（只读安全元数据） ─> ActivityService ───┘

Slint Settings（按需启动） ──> settings.json ──> 主进程低频重载
```

### 3.1 官方额度

`OfficialQuotaService` 是唯一网络数据源。它只请求官方账户、订阅额度和 Credits，失败时保留最近一次可信快照并标注更新时间；不伪造额度，不高频轮询。

### 3.2 可信活动状态

活动状态不再把 `inProgress` 直接当作“思考中”。状态机只使用无内容的数据库列：`thread_turns.status`、`started_at`、`completed_at`、`thread_items.item_type`、`created_at_ms`。

| 证据 | 显示状态 | 状态灯 |
| --- | --- | --- |
| 最新进行中项目为 `reasoning` | 推理中 | 紫色呼吸 |
| 最新进行中项目为 `commandExecution`、`fileChange`、`webSearch`、工具调用 | 执行中 | 青色呼吸 |
| App-server 收到可验证的等待输入事件 | 等待操作 | 琥珀色静态光晕 |
| 刚完成/失败（短暂提示） | 已完成/失败 | 绿色呼吸三秒 / 红色光晕 |
| 没有进行中 Turn | 空闲 | 绿色静态光晕 |
| 数据库不可读或 schema 不兼容 | 状态不可用 | 灰色，无光晕 |

不读取 `item_json`、prompt、命令正文、线程标题或线程 ID；SQLite 始终使用 `mode=ro`。

### 3.3 任务栏

- 固定显示：状态灯、weekly 剩余百分比/重置时间。
- 有 5 小时额度时使用紧凑嵌套圆环；无 5 小时额度时自动移除内环。
- Credits 仅在“有 5h 时 5h 用尽”或“无 5h 时 weekly 用尽”后替代额度显示。
- 不在任务栏文字中显示“未知状态”“API 可用”等诊断词；诊断仅在详情卡或设置中出现。
- 详情卡从任务栏上方弹出，展示账户、订阅额度、当前状态、刷新时间与历史趋势；没有 New API 相关卡片。

### 3.4 设置窗口

设置页采用独立的 **Rust + Slint** 声明式原生窗口：仅在用户从托盘菜单打开时启动，关闭即退出。它不参与常驻任务栏的内存/CPU 占用，不使用 WebView2，也不复用旧 Win32 自绘滚动代码。

页面仅保留：

1. 任务栏布局与可见项目；
2. 详情卡与刷新策略；
3. 日志级别、数据诊断和关于。

## 4. 验收标准

1. **状态真实验证**：在真实 Codex Turn 中截取“推理中”和“执行中”各一张图；完成后 3 秒内显示绿色完成，再稳定为空闲。不得只靠单元测试宣称完成。
2. **官方额度验证**：用受控 fixture 验证 5h 存在/不存在、5h 用尽、weekly 用尽、Credits 存在/未知五种组合；真实官方登录截图至少一张。
3. **设置验证**：100%、150%、200% DPI 下可滚动、可点击、可保存、重启后保留；无文字残影。
4. **安全验证**：日志与诊断中不出现 API Key、Token、User ID、完整服务地址、prompt 或线程 ID。
5. **交付验证**：`cargo fmt --check`、全工作区测试、Clippy、release build 全部通过；提供 release exe 体积、空闲内存和实际截图。

