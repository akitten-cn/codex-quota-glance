# Changelog

## 0.1.13

- 修正官方仅保留 7 天额度时，胶囊仍重复显示一条相同 5h 额度的问题。
- 当 Codex RPC 未提供窗口时长时，使用同次会话中的额度窗口校正身份；刷新时间和百分比匹配后，正确归类为 7d。
- 保留动态切换能力：官方以后恢复 5h 窗口时，会继续自动显示 5h + 7d，无需重启或修改设置。
- 校正过程复用现有概览数据，不增加额外轮询和磁盘扫描。
- 增加缺少窗口时长的真实响应结构回归测试。

## 0.1.12

- 修正活动红绿灯误判：当前轮次仍在执行、编辑或思考时，不再因为一段时间没有新日志而自动显示为空闲；收到正常完成或中止事件后仍会立即切回空闲。
- 提升胶囊与详情窗口的置顶层级，并启用全屏工作区可见性，减少其他软件进入全屏后胶囊被隐藏的问题。
- 兼容官方临时取消 5 小时额度窗口：根据官方返回的窗口时长动态识别 5h 与 7d，只有 7d 时仅显示 7d，5h 恢复后自动切回 5h + 7d，无需重启。
- 同步修正额度恢复提醒状态：5h 暂时消失时清理旧周期，恢复后重新跟踪新的额度周期。
- 增加活动状态、全屏窗口、单 7d 窗口和额度动态切换的回归测试。

## 0.1.11

- 修正官方登录模式的今日 Token 统计：每条唯一的 `token_count` 事件都会计入累计，相同 Token 数值不再被误判为重复记录。
- 统一官方登录与 API 模式的每日用量，切换模式后今日 Token 不再清零；API 花费则从切换到 API 模式的时间点开始计算。
- 收到任务完成事件后立即将胶囊切换为空闲状态，并保留 15 秒异常兜底，减少停止后仍长时间显示“思考中”的情况。
- 新增官方 5 小时额度恢复提醒；额度恢复后弹出确认窗口，点击“知道了”后本周期不再重复提醒，直接关闭窗口则会在后续检测时再次提醒。
- 增加运行状态持久化与回归测试，覆盖跨模式累计、API 花费起点、活动状态收敛和额度恢复提醒流程。

## 0.1.10

- Fixed stale Codex activity state so the capsule returns to idle after a stopped session stops writing activity events.
- Fixed API-mode daily token and spend display when the New API access token is invalid by falling back to trusted local Codex usage data for the current day.
- Kept fast local updates through the Codex session watcher and shared overview endpoint while reducing duplicated detail-window polling.
- Added regression coverage for stale activity settling, local API-mode spend fallback, Codex overview ingestion, and refresh coordination.

## 0.1.9

- 设置页点击“更新”后会直接打开更新窗口并自动开始下载，无需在更新窗口中再次点击。
- 下载开始后隐藏更新按钮，只保留下载状态、进度条和文件大小，避免重复点击。
- 下载完成后会先退出当前运行中的 Codex Quota Glance，再启动安装程序，避免安装器提示“另一程序正在使用此文件”。
- 胶囊窗口也会静默检查更新，并在胶囊本体右上角显示更新徽标；胶囊模式不会弹出大更新提示。
- 更新窗口支持复用：如果窗口已打开，从设置页再次点击更新会通知现有窗口自动下载。
- 保持下载资产选择逻辑指向 GitHub latest release 中的最新版 Windows 安装包，例如本地 0.1.7 会下载远端 0.1.9。
- 补充自动下载、按钮隐藏、退出后安装、胶囊更新提示和发布版本的回归测试。

## 0.1.8

- 优化更新入口：设置页检测到新版本后，点击“更新”会强制打开更新窗口，不再受启动提醒关闭状态影响。
- 精简更新弹窗：移除“本次运行不再提醒”按钮，关闭窗口即可结束本次弹窗打扰。
- 改进下载反馈：点击更新后会立即显示“正在连接 GitHub”，避免网络连接阶段看起来像没有反应。
- 增加持续更新提示：设置页“关于/更新”标签会显示更新小圆点，胶囊右上角也会显示轻量更新徽标。
- 修正 Codex 活动灯：工具调用结束、补丁应用结束后切回“思考中”黄灯，减少红灯滞留，更贴近 Codex 内部状态。
- 补充更新入口、更新徽标、下载反馈和活动状态映射的回归测试。

## 0.1.7

- Fixed Codex activity lights so normal thinking/commentary text no longer keeps the red executing light on; only real tool and command activity is treated as executing.
- Increased the standalone update window height so download progress and action buttons fit without clipping.
- Hid the per-run dismiss button after an update download starts, avoiding accidental interruption of an in-progress update.
- Added an Update button next to the manual update check in Settings > About/Update when a newer version is available.
- Switched installer downloads to Electron's network stack with system proxy resolution so GitHub release asset downloads can follow the user's local VPN/system proxy.
- Added regression coverage for the corrected activity mapping, update-window layout, Settings update entry point, and proxy-aware update downloads.

## 0.1.6

- Removed the aggressive desktop capsule visibility guard that could bring hidden windows back incorrectly or leave the capsule missing after long runs.
- Added desktop capsule renderer recovery and transparent-window click-through support so empty transparent regions can pass clicks to the app underneath.
- Split Codex activity into clearer thinking, executing, waiting-for-user, and finished states, and changed the traffic lights to a breathing signal effect.
- Moved update reminders into a standalone startup window with per-run dismissal shared through the Electron main process.
- Added Windows installer download support from GitHub Release assets, including download progress and automatic installer launch after download.
- Added regression coverage for capsule stability, activity-state mapping, update-window behavior, release asset selection, and the new desktop shell IPC.

## 0.1.5

- Fixed official-login quota display so the floating capsule prefers the live Codex rate-limit response instead of stale quota values embedded in local token logs.
- Reduced the official-login quota cache from 30 seconds to 5 seconds so the 5h and 7d remaining values catch up faster after Codex refreshes.
- Added regression coverage for stale local quota data being overridden by live official quota data.
- Moved the NSIS installer include script out of the generated build directory so GitHub Actions release packaging can find it reliably.
- Kept the release-notes extraction test from leaving temporary files in the repository root.

## 0.1.4

- Reworked the desktop detail card into a detached transparent window that stays prewarmed instead of repeatedly hiding and showing, reducing flicker during frequent capsule clicks.
- Fixed the capsule window accidentally becoming transparent after the detail-window opacity change, and added regression coverage so capsule opacity cannot be disabled again.
- Tightened edge positioning for the detail card by using the real measured card height instead of a large fixed fallback height.
- Added local Codex token summary estimation for today's API-mode spend, so the capsule and detail panel update from local SQLite data first while platform logs remain a slower calibration source.
- Renamed the misleading "page refresh" setting to "local summary refresh" and clarified that it only rereads local SQLite summaries, not the platform API.
- Strengthened tests around detached detail layout, local spend estimation, update copy, and desktop window behavior.

## 0.1.3

- Fixed the floating capsule anchor timing when opening and closing the detail panel near the screen edge.
- Added a two-phase desktop window layout handshake to avoid one-frame capsule jumps during top-side popover transitions.
- Added an About/Update settings page with current version, GitHub links, manual update check, and per-run update reminder dismissal.
- Switched update checking to the local Electron backend and GitHub Releases latest redirect, avoiding frontend GitHub API rate-limit failures.
- Added a local backend health endpoint so packaged Electron builds do not accidentally attach to legacy temporary backends.
- Cleaned generated artifacts more aggressively, including old `release-electron` and `local-server.exe` leftovers.
- Fixed mojibake text in settings and local pricing UI.

## 0.1.2

- Fixed GitHub Actions release packaging by disabling electron-builder's implicit publish step.
- Release assets are uploaded by `softprops/action-gh-release` using the workflow `GITHUB_TOKEN`.

## 0.1.1

- Migrated the runtime backend from Python sidecar to Electron/Node.
- Removed legacy PowerShell packaging scripts and Tauri experiment files.
- Switched the main release flow to `electron-builder`.
- Added GitHub Releases update checking in the About page.
- Added Node-based secret scanning and generated-file cleanup scripts.
- Fixed packaged app startup path handling for `app.asar`.

## 0.1.0

Initial public release candidate.

- Windows Electron floating capsule.
- Codex official-login quota display.
- Codex API mode provider auto-selection by local key fingerprint.
- New API provider management.
- SQLite-backed New API log cache.
- Daily token and cost display.
- Local token cost estimation.
- GitHub Actions CI and release packaging.
