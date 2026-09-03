# 实施计划（Plan）

## 总体依赖

```text
需求冻结
  ├─ Domain 状态模型
  ├─ Windows P0 探针
  ├─ App Server 协议 adapter
  └─ Settings / Diagnostics
          ↓
      Application 状态仲裁
          ↓
      Direct2D Renderer
          ↓
      任务栏 + 托盘 + 浮窗集成
          ↓
      兼容性/资源基准
          ↓
      P1 功能实现
```

## P0：原生可行性门禁

### P0.1 Windows Geometry 与发现

- 枚举主/副任务栏与目标显示器。
- 解析左侧系统组件和右侧通知区域。
- Left/Right、安全间距、TrafficMonitor 预留偏移。
- 纯函数坐标测试。

### P0.2 透明任务栏子窗口

- 原生无边框 HWND、DPI、消息循环。
- 假数据嵌套圆环和光晕灯。
- Explorer 子窗口挂接、逐像素 alpha 和无背景元素绘制。
- Explorer 重启与显示器变化恢复。

当前状态：已实现并在 2026-08-23 的 Windows 双屏环境完成透明任务栏、详情卡与设置窗口实机截图；自动化覆盖坐标、DPI 和状态机。Explorer 重启、显示器拔插、四向任务栏与完整混合 DPI 矩阵仍需继续验收。

### P0.3 通过标准

- 不覆盖通知区、时钟、开始菜单或现有 TrafficMonitor。
- 主副屏和混合 DPI 可恢复。
- 8 小时无持续增长，静态状态不持续重绘。

未通过 P0 时停止扩展业务功能，保留托盘/桌面浮窗路线。

## P1：核心产品

### P1.1 Codex 数据

- App Server 生命周期和能力探测。
- 额度完整快照 + sparse 通知。
- thread/turn/item 活动事件。
- thread/account Token usage。
- 断线、退避和显式 freshness。

当前状态：协议、后台会话、LocalAppData CLI 定位和 SQLite 只读后备已实现；App Server 能力探测与长期会话均以 `CREATE_NO_WINDOW` 启动。已兼容本机 snake_case Token 明细、手动刷新、活动状态优先级和 `account/tokenUsage/read` 的线程级官方 USD/credits 估算。独立 App Server 无法获得桌面进程既有 stdio 事件时仍会使用 SQLite 低置信后备，并明确 freshness。

### P1.2 UI

- 嵌套额度环。
- 带光晕的状态灯和低功耗动画 scheduler。
- 可排序显示项与空间折叠。
- 重置倒计时和基础 Token 文本。
- 托盘、手动刷新和诊断状态。

当前状态：V2 主路径已可用。任务栏具备嵌套额度环、中心百分比、带光晕状态灯、双行摘要、真实显隐/排序和按宽度优先级折叠；5h 缺失时内环、摘要和详情行同时隐藏。点击后打开任务栏上方双栏详情卡，悬停或 Turn 结算边界显示约 4 秒 Token 快览。官方详情卡已实现保形平滑面积曲线、真实日桶 hover/Tooltip、K/M 自动进位、Token 构成图、官方费用估算和独立 freshness；详情卡刷新/设置按钮及托盘命令映射已实现。仍待托盘菜单逐项点击、Explorer 重启和混合 DPI 完整人工证据。

### P1.3 设置与日志

- 显示器、锚点、宽度、间距、偏移。
- 显示项排序和隐藏。
- 日志等级热更新、滚动和脱敏导出基础。
- 配置 schema migration。

当前状态：原生设置页已经分页为“任务栏/数据源”，支持目标显示器、左右锚点、宽度、间距、TrafficMonitor 避让、减少动画、日志等级、显示项显隐/排序，以及 New API 端点、轮询、手动额度换算和明确的凭据保留/替换/清除动作；已有秘密不会回显。窗口定位到调用宿主所在显示器并 clamp 到工作区，高 DPI/小屏使用纵向滚动；保存采用原子落盘并热应用。

## P2：可靠性与发布

- 开机启动、单实例、崩溃恢复。
- Portable 与安装版。
- Explorer/显示器/DPI 完整矩阵。
- 内存、CPU、启动和包体实测。
- 版本信息、签名与更新策略。

## P3：暂缓功能

- 完整历史浏览器、费用预测、模型分布和多 Provider 聚合；基础五日真实趋势已经实现。
- WebView2 设置页。
- 插件系统和跨平台 UI。

## 下一阶段工作包

| 工作包 | 模型 | 难度 | 修改边界 |
|---|---|---:|---|
| Explorer/多屏/DPI/托盘菜单人工矩阵 | 主 Agent | 高 | 测试与验收产物 |
| 设置页 Direct2D/自绘现代化视觉迭代 | 5.6-luna xhigh | 中 | `platform-windows/settings_ui` |
| 安装包、单实例与开机启动 | 5.6-terra high | 高 | 发布与 Windows 集成 |

## 每个工作包的完成定义

- 文件和公共 API 有中文职责注释。
- 重要状态/兼容规则有测试，不以源码字符串匹配代替行为测试。
- 不读取或提交真实用户数据。
- `cargo fmt`、`cargo test` 通过。
- 不覆盖其他 Agent 或用户已有修改。
- 回传已知限制和下一阶段依赖。


