# 性能与资源稳定性测量

本项目把“固定资源成本”和“持续增长”分开验收。Direct2D、DirectWrite 与 Windows
字体栈会增加主进程 Working Set，但这不等同于泄漏；泄漏必须由连续样本中的
Private Bytes、句柄、GDI/USER 对象或线程持续增长来证明。

## 标准采样入口

先构建 Release：

```powershell
cargo build --release
```

进行 15 分钟真实静态空闲采样：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\measure-runtime.ps1 `
  -Mode visual-preview-idle -DurationSeconds 900 -IntervalSeconds 15
```

进行持续执行动画采样：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\measure-runtime.ps1 `
  -Mode visual-preview -DurationSeconds 600 -IntervalSeconds 10
```

脚本只使用固定脱敏预览数据，不读取真实 Codex/New API 数据。每次生成：

- `artifacts/performance/*.csv`：逐样本原始数据；
- `artifacts/performance/*.summary.json`：首末值、峰值、差值和端点增长率。

除非显式使用 `-KeepRunning`，脚本只会结束它自己启动的精确 PID，不会按进程名
清理其他实例，也不会修改正式 `settings.json`。

## 指标口径

| 指标 | 用途 |
|---|---|
| Working Set | 当前驻留物理内存，可能因 Windows 内存回收上下波动 |
| Private Bytes | 进程私有提交内存，更适合观察持续堆增长 |
| normalized CPU | 除以逻辑处理器数量后的整机占用百分比 |
| thread count | 检查 worker、图形栈线程是否持续增加 |
| handle count | 检查内核对象、文件、同步对象等是否持续增加 |
| GDI objects | 检查 DIB、DC、字体和画刷等 GDI 资源 |
| USER objects | 检查 HWND、菜单等窗口管理资源 |

端点增长率只是首末样本的线性速率提示，短样本容易被初始化和 Windows 回收行为
放大。判断泄漏时必须同时查看完整 CSV、15/60 分钟样本以及重复打开/关闭浮层后的
句柄稳定性，不能仅凭一个正的 `MiB/hour` 数字下结论。

## 当前边界

- `--visual-preview-idle` 的 Idle 状态先呼吸三秒，然后原生动画定时器停止；这是
  静态空闲基线。
- `--visual-preview` 使用 Executing 状态并持续 20 FPS 呼吸；这是动画期间基线，
  不能再标成“静态空闲”。
- 正式模式还会启动独立 `codex.exe app-server` 子进程；主进程与子进程必须分别
  记录，不能只展示合计后误判 UI 壳本身。
- Explorer 重启和显示器拔插属于有桌面扰动的验收，执行前需得到用户明确同意。

## 2026-08-23 软件渲染 Idle 基线

样本：`artifacts/performance/runtime-visual-preview-idle-20260823-103230.csv`，
请求 900 秒、实际取得 60 个样本。验收实例在最后阶段被正常替换，因此摘要中的
“首末端点增长率”和线程末值不能单独用于判断泄漏；稳定区间应从初始化完成后的
第 60 秒开始观察。

第 60 秒至最后一个样本：

| 指标 | 范围/变化 |
|---|---|
| Working Set | 约 27.8–28.6 MiB；首末约 -0.40 MiB |
| Private Bytes | 约 13.8–15.5 MiB；首末约 +0.29 MiB |
| normalized CPU | 平均接近 0%，稳定区间峰值约 0.013% |
| Handle | 233–237；首末 -4 |
| GDI | 恒定 13 |
| USER | 恒定 6 |

结论：当前约 14 分钟稳定区间未出现单调内存、句柄或 GDI/USER 对象增长；它是
“短期空闲稳定性通过”，不是 60 分钟/8 小时稳定性结论。

## 2026-08-24 Release 与便携包基线

| 产物 | 大小 |
|---|---:|
| `target/release/codex-taskbar.exe` | 2,320,384 bytes（约 2.21 MiB） |
| `dist/codex-taskbar-0.1.0-windows-x64-portable.zip` | 1,260,297 bytes（约 1.20 MiB） |

便携包只包含主程序和 UTF-8 `使用说明.txt`，不捆绑 WebView2、Electron 或独立运行时。
安装包、冷启动和包含正式 App Server 子进程的长期资源数据仍需在后续验收。
