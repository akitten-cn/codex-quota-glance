# Tauri 迁移 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将现有 Electron 桌面壳迁移为 Tauri 架构，尽量复用 React UI，并完成胶囊更新提醒小橙点优化。

**Architecture:** 前端继续使用 React/Vite，桌面窗口、更新下载和本地能力逐步迁到 Tauri Rust command。迁移期间保留 Electron 文件作为回退参照，但新增 Tauri 路径必须能独立 build，并逐步替代默认打包路径。

**Tech Stack:** React 18、Vite、Tauri 2、Rust、pnpm、GitHub Actions。

---

### Task 1: 建立迁移分支与基线

**Files:**
- Modify: `package.json`
- Modify: `scripts/run-tests.mjs`

- [x] 创建 `feature/tauri-migration` 分支。
- [x] 跑迁移前测试基线：`node scripts/run-tests.mjs`。
- [x] 切换本地依赖管理到 pnpm，并保证 CI 同步。

### Task 2: Tauri 最小壳

**Files:**
- Create: `src-tauri/Cargo.toml`
- Create: `src-tauri/tauri.conf.json`
- Create: `src-tauri/build.rs`
- Create: `src-tauri/src/main.rs`
- Create: `src-tauri/src/lib.rs`
- Create: `src-tauri/capabilities/default.json`
- Create: `src/lib/desktopBridge.ts`
- Modify: `src/main.tsx`
- Test: `tests/tauri-migration-source.test.mjs`

- [x] 添加 Tauri Rust 工程骨架。
- [x] 添加 `window.codexQuotaDesktop` 的 Tauri 兼容桥接。
- [x] 编译验证：`cargo check`。
- [x] 打包验证：`pnpm run tauri:build`。

### Task 3: 桌面窗口与更新流程迁移

**Files:**
- Modify: `src-tauri/src/lib.rs`
- Modify: `src/lib/desktopBridge.ts`
- Test: `tests/tauri-migration-source.test.mjs`

- [x] 支持胶囊窗口透明、无边框、置顶。
- [x] 支持详情窗口、设置窗口、更新窗口的 Tauri command 创建。
- [x] 支持设置页打开更新窗口并自动下载。
- [x] 支持 Rust 下载 GitHub Release 安装包、启动安装器并退出当前程序。
- [x] 补托盘菜单：显示/隐藏胶囊、打开设置、退出。
- [ ] 补窗口布局细节：胶囊尺寸随内容变化、详情窗口相对胶囊定位、点击穿透。

### Task 4: 本地 API 迁移

**Files:**
- Modify: `src-tauri/src/lib.rs`
- Modify: `src/lib/desktopBridge.ts`
- Test: `tests/tauri-migration-source.test.mjs`

- [x] 在 Tauri 环境下拦截 `/local-api/*` fetch，转发到 Rust command。
- [x] 实现 `/local-api/health`。
- [x] 实现 `/local-api/update/latest`。
- [x] 实现 Codex 最近活动解析，让红绿灯能从 `.codex/sessions/**/*.jsonl` 获取 thinking/executing/waiting/finished。
- [ ] 迁移 Codex token latest/summary 的真实统计。
- [ ] 迁移 New API logs summary/sync/diagnose 的 SQLite 与 HTTP 同步逻辑。
- [ ] 迁移 `/newapi-proxy`。

### Task 5: 胶囊更新提醒 UI

**Files:**
- Modify: `src/components/FloatingCapsule.tsx`
- Modify: `src/styles.css`
- Test: `tests/about-update-source.test.mjs`

- [x] 将胶囊更新提醒从箭头徽标改为小橙点。
- [x] 保留无障碍 label/title。

### Task 6: 默认构建与发布切换

**Files:**
- Modify: `package.json`
- Modify: `.github/workflows/release.yml`
- Modify: `README.md`

- [ ] 将默认 `dist`/Release workflow 从 Electron 切到 Tauri。
- [ ] 保留 Electron legacy 脚本，直到 Tauri 本地 API 功能完整。
- [ ] 更新 README 的安装包说明和本地开发命令。

### Task 7: 完整验证

- [x] `pnpm test`
- [x] `pnpm run build`
- [x] `cargo check`
- [x] `pnpm run tauri:build`
- [x] 启动 Tauri exe 做基础进程 smoke test。
- [x] 对比 Tauri 包体积：主 exe 约 11.92 MB，NSIS 安装器约 2.93 MB，MSI 约 4.22 MB。
- [ ] 做窗口/更新/红绿灯人工交互 smoke test。
- [ ] 对比 Electron/Tauri 进程内存。
