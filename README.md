# Codex Taskbar

Codex 额度、Token 消耗与运行状态监视器。仓库沿用 `codex-quota-glance` 地址，当前主分支为 Rust + WebView2 版本。

## 分支与平台

- `main`：当前版本及后续 Windows/macOS 共用代码的开发主线。
- [`archive/electron-before-rust-20260903`](https://github.com/akitten-cn/codex-quota-glance/tree/archive/electron-before-rust-20260903)：旧 Electron 版本，保留旧主分支历史和迁移时本地未发布的修复。
- Windows：当前支持任务栏胶囊、详情卡片、消耗弹窗与应用内设置页。
- macOS：`feature/macos-floating` 提供 AppKit/WKWebView 桌面浮动测试版，支持 arm64 与 Intel 分别构建。详情和设置属于同一个 `.app`；共享 Rust 引擎通过应用内部管道提供真实数据。仍需要用户实机验收，不能将 Windows EXE 复制到 Mac 运行。

已有 GitHub Releases 仍为其各自版本的产物，不能据此判断当前主分支的实现。源码迁移不会发布新版本或更新已安装的软件。

## 当前架构

- `crates/domain`、`crates/application`：额度、活动、统计和本机用量账本。
- `crates/adapters-codex-app-server`、`crates/adapters-codex-sqlite`：账户/额度读取与本机数据适配。
- `crates/settings`：配置及 SQLite 持久化。
- `crates/platform-windows`：Windows 任务栏、原生窗口与系统集成。
- `apps/codex-taskbar`：应用装配、数据运行时、WebView2 宿主与更新流程。
- `apps/codex-taskbar-settings`：应用内设置功能，不作为独立软件发布。
- `prototypes`：共用的 HTML/WebGL 界面资源及设计参考。

输入 Token 包含缓存输入；总消耗按输入 + 输出计算，缓存命中率按缓存输入 / 输入计算，不重复加缓存。API 等价金额仅为估算，不是订阅账单。数据只反映已获取来源的覆盖范围；本机历史与设置不会自动跨设备同步。

## Windows 开发与打包

需要当前 Rust stable、MSVC C++ 构建工具及 WebView2 Runtime。

```powershell
cargo test --workspace --locked
cargo run --package codex-taskbar
./scripts/package-portable.ps1
```

产物位于 `dist/`。配置、账本和日志默认保存在当前用户的应用数据目录；测试可以通过 `CODEX_TASKBAR_DATA_DIR` 指定独立目录。不要将认证凭据、数据库、运行日志或 WebView 缓存提交到仓库。

推送 `main` 或提交 PR 时运行 Windows 检查；`release.yml` 可手动打包，只有推送版本标签才发布 Release。`macos-test.yml` 在适配分支分别运行 Apple Silicon 与 Intel 测试、打包和真实 WKWebView 设置保存/重开冒烟。测试包只有临时签名，没有 Apple 公证，Mac 自动安装更新尚未开放；见 [Mac 测试说明](docs/macos-testing.md)。

部分 `docs/` 文档是历史阶段记录，不代表所有描述仍是当前实现。迁移说明见 [仓库主线迁移](docs/repository-migration-20260903.md)。
