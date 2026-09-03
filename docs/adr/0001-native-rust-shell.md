# ADR-0001：Rust 原生常驻壳

- 状态：Accepted
- 日期：2026-08-22

## 决策

常驻核心采用 Rust + `windows-rs` + Direct2D/DirectWrite。任务栏信息条、浮窗和托盘均使用原生 HWND；WebView2 不属于常驻依赖，只可用于后续按需设置/历史窗口。

## 原因

- Windows 任务栏嵌入最终仍需要 Win32 HWND、DPI、Explorer 重启和多显示器处理。
- Electron 的 Chromium 多进程成本与低资源目标冲突。
- WinUI 3 自包含发布体积较大；C++ 更轻，但当前长期维护和内存安全成本更高。
- Rust 可以保持接近 C++ 的资源上限，同时用强类型状态模型减少旧版的陈旧数据合并错误。

## 风险

Explorer 没有公开的任意实时控件嵌入 API。任务栏模式属于 best-effort，必须保留托盘和桌面浮窗回退；P0 探针通过多屏、DPI、Explorer 重启和长时间空闲测试后，才进入完整功能开发。

